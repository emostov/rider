//! The command line: parse the arguments, read the environment once, and run one command.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::board::{TaskId, VbCli};
use crate::daemon::{self, DaemonError, Paths, take_lock};
use crate::harness::{CommandLine, Exit, HarnessError};
use crate::pool::{
    Effects, Event, Gate, PAUSE_STEP_MILLIS, Slot, SlotEnd, event_line, slot_loop, slots,
};
use crate::reap::{self, ReapError, Stopped};
use crate::roles::{AGENTS_MAX, PROFILE_LEN_MAX};
use crate::run_task::{Env, RunError, launch_agent, run_task};
use crate::settle::Verdict;

/// What `rider --help` prints.
pub const USAGE: &str = r#"usage:
  rider serve              run the local board in the foreground
  rider daemon             claim and run tasks on every work/ channel in the foreground;
                           SIGTERM or Ctrl-C drains, a second one stops every agent now
  rider pool <project>     run worker slots on the channel work/<project>;
                           type stop or press Ctrl-D to stop after running tasks finish
  rider run-task <id>      one attempt at a task already claimed by $BOARD_PROFILE
  rider reap [--yes]       list orphaned vb processes whose board no longer answers;
                           --yes stops them

environment:
  RIDER_HOME     rider's files, an absolute path (default $HOME/.rider)
  BOARD_URL      the board's API (default http://127.0.0.1:4100/api/board)
  BOARD_PROFILE  the profile that claimed the task, for run-task"#;

/// The longest project name.
pub const PROJECT_LEN_MAX: usize = 64;
/// Upper bound on the arguments rider looks at; every command takes at most one.
pub const ARGS_MAX: usize = 4;
/// Upper bound on what `rider pool` reads from standard input before it stops.
pub const STDIN_BYTES_MAX: u64 = 1024 * 1024;
/// Every role's prompt file and the files they read, once each, built into the binary.
pub const PROMPTS: [(&str, &str); 6] = [
    ("implement.md", include_str!("../prompts/implement.md")),
    ("review.md", include_str!("../prompts/review.md")),
    ("polish.md", include_str!("../prompts/polish.md")),
    ("security.md", include_str!("../prompts/security.md")),
    ("slop-rules.md", include_str!("../prompts/slop-rules.md")),
    ("qa.md", include_str!("../prompts/qa.md")),
];
const DEFAULT_BOARD_URL: &str = "http://127.0.0.1:4100/api/board";
const HOME_DEFAULT_DIR: &str = ".rider";
const PROMPTS_DIR: &str = "prompts";
const BOARD_DIR: &str = "board";
const VB_DIR: &str = "node/node_modules/.bin";
const ID_DIGITS_MAX: usize = 20;

/// A project name that is safe in a channel name: letters, digits, `-`, `_` and `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project(String);

impl Project {
    fn parse(text: &str) -> Result<Project, UsageError> {
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        let valid = !text.is_empty()
            && text.len() <= PROJECT_LEN_MAX
            && text.chars().all(allowed)
            && !text.starts_with(['-', '.']);
        if !valid {
            return Err(UsageError::BadProject(text.to_string()));
        }
        assert!(text.is_ascii() && text.len() <= PROJECT_LEN_MAX);
        Ok(Project(text.to_string()))
    }

    /// The channel the project's tasks are on, `work/<project>`.
    pub fn channel(&self) -> String {
        assert!(!self.0.is_empty() && !self.0.contains('/'));
        format!("work/{}", self.0)
    }
}

/// One command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Serve,
    Daemon,
    Pool(Project),
    RunTask(TaskId),
    /// Lists leaked Board CLI processes; `yes` stops them too.
    Reap {
        yes: bool,
    },
}

/// Arguments rider does not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    NoCommand,
    UnknownCommand(String),
    MissingArgument(&'static str),
    ExtraArgument(String),
    BadProject(String),
    BadTaskId(String),
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageError::NoCommand => write!(f, "no command given"),
            UsageError::UnknownCommand(word) => write!(f, "{word:?} is not a command"),
            UsageError::MissingArgument(what) => write!(f, "the {what} is missing"),
            UsageError::ExtraArgument(word) => write!(f, "{word:?} is one argument too many"),
            UsageError::BadProject(text) => write!(
                f,
                "{text:?} is not a project name: use 1 to {PROJECT_LEN_MAX} letters, digits, \
                 '-', '_' or '.', not starting with '-' or '.'"
            ),
            UsageError::BadTaskId(text) => {
                write!(
                    f,
                    "{text:?} is not a task id: use its number, such as 42 or #42"
                )
            }
        }
    }
}

/// Parses the arguments that follow the program name.
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    assert!(
        args.len() <= ARGS_MAX,
        "main passes at most ARGS_MAX arguments"
    );
    let Some((word, rest)) = args.split_first() else {
        return Err(UsageError::NoCommand);
    };
    let (command, used) = match word.as_str() {
        "--help" | "-h" | "help" => (Command::Help, 0),
        "serve" => (Command::Serve, 0),
        "daemon" => (Command::Daemon, 0),
        "pool" => {
            let project = rest.first().ok_or(UsageError::MissingArgument("project"))?;
            (Command::Pool(Project::parse(project)?), 1)
        }
        "reap" => match rest.first().map(String::as_str) {
            Some("--yes") => (Command::Reap { yes: true }, 1),
            _ => (Command::Reap { yes: false }, 0),
        },
        "run-task" => {
            let id = rest.first().ok_or(UsageError::MissingArgument("task id"))?;
            (Command::RunTask(parse_task_id(id)?), 1)
        }
        _ => return Err(UsageError::UnknownCommand(word.clone())),
    };
    if let Some(extra) = rest.get(used) {
        return Err(UsageError::ExtraArgument(extra.clone()));
    }
    assert!(rest.len() == used);
    Ok(command)
}

fn parse_task_id(text: &str) -> Result<TaskId, UsageError> {
    let bad = || UsageError::BadTaskId(text.to_string());
    let digits = text.strip_prefix('#').unwrap_or(text);
    if digits.is_empty()
        || digits.len() > ID_DIGITS_MAX
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let id = digits.parse::<u64>().map_err(|_| bad())?;
    assert!(id.to_string().len() <= digits.len());
    Ok(TaskId(id))
}

/// The environment variables rider reads, as they were found.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    pub rider_home: Option<OsString>,
    pub home: Option<OsString>,
    pub board_url: Option<OsString>,
    pub path: Option<OsString>,
    pub board_profile: Option<OsString>,
}

/// Reads every environment variable rider uses, once.
pub fn read_vars() -> Vars {
    Vars {
        rider_home: env::var_os("RIDER_HOME"),
        home: env::var_os("HOME"),
        board_url: env::var_os("BOARD_URL"),
        path: env::var_os("PATH"),
        board_profile: env::var_os("BOARD_PROFILE"),
    }
}

/// Where rider's files are and how to reach the board, checked once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Absolute and UTF-8.
    pub home: PathBuf,
    pub board_url: String,
    pub inherited_path: Option<String>,
    pub board_profile: Option<String>,
}

impl Config {
    fn vb_dir(&self) -> PathBuf {
        self.home.join(VB_DIR)
    }

    fn prompts(&self) -> PathBuf {
        self.home.join(PROMPTS_DIR)
    }
}

/// An environment variable rider cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    NoHome,
    NotAbsolute {
        var: &'static str,
        value: String,
    },
    NotUnicode(&'static str),
    Empty(&'static str),
    /// The home holds `:`, which would split the agents' `PATH`.
    HomeHasColon(String),
    BadProfile(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoHome => write!(f, "neither RIDER_HOME nor HOME is set"),
            ConfigError::NotAbsolute { var, value } => {
                write!(f, "{var} must be an absolute path, not {value:?}")
            }
            ConfigError::NotUnicode(var) => write!(f, "{var} is not valid UTF-8"),
            ConfigError::Empty(var) => write!(f, "{var} is set but empty"),
            ConfigError::HomeHasColon(home) => {
                write!(f, "rider's home {home:?} must not contain ':'")
            }
            ConfigError::BadProfile(profile) => write!(
                f,
                "BOARD_PROFILE {profile:?} is not a profile: use at most {PROFILE_LEN_MAX} \
                 letters, digits and '-'"
            ),
        }
    }
}

/// Checks the environment variables and fills in defaults.
pub fn config(vars: Vars) -> Result<Config, ConfigError> {
    let text = |value: Option<OsString>, var: &'static str| match value {
        None => Ok(None),
        Some(value) => match value.into_string() {
            Ok(text) if text.is_empty() && var != "PATH" => Err(ConfigError::Empty(var)),
            Ok(text) => Ok(Some(text)),
            Err(_) => Err(ConfigError::NotUnicode(var)),
        },
    };
    let home = match (
        text(vars.rider_home, "RIDER_HOME")?,
        text(vars.home, "HOME")?,
    ) {
        (Some(rider_home), _) => (PathBuf::from(&rider_home), "RIDER_HOME", rider_home),
        (None, Some(home)) => (Path::new(&home).join(HOME_DEFAULT_DIR), "HOME", home),
        (None, None) => return Err(ConfigError::NoHome),
    };
    let (home, var, value) = home;
    if !home.is_absolute() {
        return Err(ConfigError::NotAbsolute { var, value });
    }
    if value.contains(':') {
        return Err(ConfigError::HomeHasColon(value));
    }
    let board_profile = text(vars.board_profile, "BOARD_PROFILE")?;
    if let Some(profile) = &board_profile {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || b == b'-';
        if profile.len() > PROFILE_LEN_MAX || !profile.bytes().all(allowed) {
            return Err(ConfigError::BadProfile(profile.clone()));
        }
    }
    let board_url = text(vars.board_url, "BOARD_URL")?;
    let config = Config {
        home,
        board_url: board_url.unwrap_or_else(|| DEFAULT_BOARD_URL.to_string()),
        inherited_path: text(vars.path, "PATH")?,
        board_profile,
    };
    assert!(config.home.is_absolute() && config.home.to_str().is_some());
    assert!(!config.board_url.is_empty());
    Ok(config)
}

/// `cmd` with `vb_dir` put first on its `PATH`, ahead of `inherited` when there is one.
pub fn with_vb_on_path(cmd: &CommandLine, vb_dir: &str, inherited: Option<&str>) -> CommandLine {
    assert!(!vb_dir.is_empty() && !vb_dir.contains(':'));
    let path = match inherited {
        Some(inherited) if !inherited.is_empty() => format!("{vb_dir}:{inherited}"),
        _ => vb_dir.to_string(),
    };
    let mut with = cmd.clone();
    with.env.retain(|(name, _)| name != "PATH");
    with.env.push(("PATH".to_string(), path));
    assert!(with.env.iter().filter(|(name, _)| name == "PATH").count() == 1);
    with
}

/// Whether a line typed to `rider pool` asks it to stop.
pub fn is_stop(line: &str) -> bool {
    line.trim() == "stop"
}

/// A command that could not finish.
#[derive(Debug)]
pub enum CliError {
    Config(ConfigError),
    NoProfile,
    Io {
        action: &'static str,
        path: PathBuf,
        message: String,
    },
    Run(RunError),
    SlotsFailed(usize),
    Daemon(DaemonError),
    Reap(ReapError),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Config(error) => write!(f, "{error}"),
            CliError::NoProfile => {
                write!(
                    f,
                    "run-task needs BOARD_PROFILE, the profile that claimed the task"
                )
            }
            CliError::Io {
                action,
                path,
                message,
            } => write!(f, "could not {action} {}: {message}", path.display()),
            CliError::Run(RunError::Board(error)) => write!(f, "{error}"),
            CliError::Run(RunError::LostClaim(state)) => {
                write!(f, "the task is no longer claimed; it is {state:?}")
            }
            CliError::Daemon(error) => write!(f, "{error}"),
            CliError::Reap(error) => write!(f, "{error}"),
            CliError::SlotsFailed(count) => {
                write!(f, "{count} slots stopped before they were asked to")
            }
        }
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> CliError {
        CliError::Config(error)
    }
}

/// Writes every role's prompt under the prompts directory, replacing what is there.
pub fn write_prompts(config: &Config) -> Result<(), CliError> {
    let dir = config.prompts();
    let io = |action, path: &Path| {
        let path = path.to_path_buf();
        move |error: io::Error| CliError::Io {
            action,
            path,
            message: error.to_string(),
        }
    };
    fs::create_dir_all(&dir).map_err(io("create", &dir))?;
    for (name, text) in PROMPTS.iter() {
        assert!(!name.contains('/') && !text.is_empty());
        let path = dir.join(name);
        fs::write(&path, text).map_err(io("write", &path))?;
    }
    Ok(())
}

/// Runs `command` and returns the exit code rider should end with.
pub fn execute(command: &Command, config: &Config) -> Result<u8, CliError> {
    assert!(config.home.is_absolute());
    match command {
        Command::Help => {
            say(USAGE);
            Ok(0)
        }
        Command::Serve => serve(config),
        Command::Daemon => daemon(config),
        Command::Pool(project) => pool(project, config),
        Command::RunTask(id) => run_one(*id, config),
        Command::Reap { yes } => reap_leaks(*yes, config),
    }
}

fn reap_leaks(yes: bool, config: &Config) -> Result<u8, CliError> {
    let leaks = reap::find_leaks(&config.board_url).map_err(CliError::Reap)?;
    for leak in &leaks {
        say(&reap::leak_line(leak));
    }
    let count = leaks.len();
    if !yes {
        say(&format!(
            "{count} leaked vb processes; rider reap --yes stops them"
        ));
        return Ok(0);
    }
    let ends = reap::stop(&leaks);
    assert_eq!(ends.len(), count);
    let mut failed = 0_usize;
    for (leak, end) in leaks.iter().zip(&ends) {
        match end {
            Ok(Stopped::Gone) => say(&format!("pid {} had already ended", leak.pid)),
            Ok(Stopped::Term) => say(&format!("stopped pid {} with SIGTERM", leak.pid)),
            Ok(Stopped::Kill) => say(&format!("stopped pid {} with SIGKILL", leak.pid)),
            Err(error) => {
                failed += 1;
                say(&format!("could not stop pid {}: {error}", leak.pid));
            }
        }
    }
    say(&format!(
        "stopped {} of {count} leaked vb processes",
        count - failed
    ));
    Ok(u8::from(failed > 0))
}

fn serve(config: &Config) -> Result<u8, CliError> {
    let dir = config.home.join(BOARD_DIR);
    let io = |action, path: PathBuf| {
        move |error: io::Error| CliError::Io {
            action,
            path,
            message: error.to_string(),
        }
    };
    fs::create_dir_all(&dir).map_err(io("create", dir.clone()))?;
    let vb = config.vb_dir().join("vb");
    // The board runs in the foreground, so it shares rider's terminal, standard input included.
    let status = Process::new(&vb)
        .arg("serve")
        .arg("--dir")
        .arg(&dir)
        .arg("--no-open")
        .status()
        .map_err(io("run", vb))?;
    assert!(dir.is_absolute());
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

fn daemon(config: &Config) -> Result<u8, CliError> {
    write_prompts(config)?;
    let paths = Paths {
        home: config.home.clone(),
        prompts: config.prompts(),
        vb: config.vb_dir().join("vb"),
        board_url: config.board_url.clone(),
        inherited_path: config.inherited_path.clone(),
    };
    assert!(paths.vb.is_absolute() && paths.prompts.is_absolute());
    daemon::run(paths).map_err(CliError::Daemon)
}

fn pool(project: &Project, config: &Config) -> Result<u8, CliError> {
    let _lock = take_lock(&config.home).map_err(CliError::Daemon)?;
    write_prompts(config)?;
    let channel = project.channel();
    let stop = Arc::new(AtomicBool::new(false));
    watch_stdin(Arc::clone(&stop));
    let all = slots();
    say(&format!(
        "rider pool: {} slots on {channel}, at most {AGENTS_MAX} agents at once; \
         type stop or press Ctrl-D to stop after running tasks finish",
        all.len()
    ));
    let gate = Gate::new();
    let ends: Vec<bool> = thread::scope(|scope| {
        let handles: Vec<_> = all
            .iter()
            .map(|slot| {
                let (channel, gate, stop) = (channel.as_str(), &gate, &*stop);
                scope.spawn(move || run_slot(slot, channel, gate, stop, config))
            })
            .collect();
        let joined = handles.into_iter().map(|handle| handle.join());
        joined
            .map(|end| matches!(end, Ok(SlotEnd::Stopped)))
            .collect()
    });
    assert_eq!(ends.len(), all.len());
    let failed = ends.iter().filter(|stopped| !**stopped).count();
    if failed > 0 {
        return Err(CliError::SlotsFailed(failed));
    }
    Ok(0)
}

fn run_slot(
    slot: &Slot,
    channel: &str,
    gate: &Gate,
    stop: &AtomicBool,
    config: &Config,
) -> SlotEnd {
    let board = VbCli {
        vb: config.vb_dir().join("vb"),
        url: config.board_url.clone(),
        profile: slot.profile.clone(),
    };
    let env = Env {
        home: config.home.clone(),
        prompts: config.prompts(),
        profile: slot.profile.clone(),
        board_url: config.board_url.clone(),
    };
    let launch = launcher(config);
    let run = |id: TaskId| run_task(id, &env, &board, &launch);
    let pause = || thread::sleep(Duration::from_millis(PAUSE_STEP_MILLIS));
    let effects = Effects {
        run: &run,
        pause: &pause,
        say: &say,
        now: &SystemTime::now,
    };
    slot_loop(slot, channel, &board, gate, stop, &effects)
}

fn run_one(id: TaskId, config: &Config) -> Result<u8, CliError> {
    let profile = config.board_profile.as_ref().ok_or(CliError::NoProfile)?;
    assert!(!profile.is_empty());
    write_prompts(config)?;
    let board = VbCli {
        vb: config.vb_dir().join("vb"),
        url: config.board_url.clone(),
        profile: profile.clone(),
    };
    let env = Env {
        home: config.home.clone(),
        prompts: config.prompts(),
        profile: profile.clone(),
        board_url: config.board_url.clone(),
    };
    let verdict = run_task(id, &env, &board, &launcher(config)).map_err(CliError::Run)?;
    let now = SystemTime::now();
    let line = match &verdict {
        Verdict::Complete { result } => {
            event_line(Event::Completed(result), Some(id), profile, now)
        }
        Verdict::Fail { reason } => event_line(Event::Failed(reason), Some(id), profile, now),
        Verdict::LeaveBlocked => event_line(Event::Blocked, Some(id), profile, now),
    };
    say(&line);
    Ok(0)
}

fn launcher(config: &Config) -> impl Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError> {
    let vb_dir = config.vb_dir().to_string_lossy().into_owned();
    let inherited = config.inherited_path.clone();
    assert!(
        config.home.to_str().is_some(),
        "the home was checked to be UTF-8"
    );
    move |cmd: &CommandLine, dir: &Path, log: &Path| {
        launch_agent(
            &with_vb_on_path(cmd, &vb_dir, inherited.as_deref()),
            dir,
            log,
        )
    }
}

fn watch_stdin(stop: Arc<AtomicBool>) {
    assert!(!stop.load(Ordering::SeqCst));
    // The thread is left blocked on standard input when the pool ends some other way; it ends
    // with the process.
    thread::spawn(move || {
        let lines = io::stdin().lock().take(STDIN_BYTES_MAX).lines();
        for line in lines {
            match line {
                Ok(line) if is_stop(&line) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        stop.store(true, Ordering::SeqCst);
        say("stopping: each slot finishes the task it is running and claims no more");
    });
}

fn say(line: &str) {
    assert!(!line.is_empty());
    let mut out = io::stdout().lock();
    // A closed standard output must not stop the agents' work, so a failed write is dropped.
    let _ = writeln!(out, "{line}");
}

#[cfg(test)]
mod tests {
    use crate::test_support::write_executable;
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::roles::ROLES;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn project(name: &str) -> Project {
        Project(name.to_string())
    }

    fn scratch(name: &str) -> PathBuf {
        let unique = format!("rider-cli-unit-{name}-{}", std::process::id());
        let dir = std::env::temp_dir().join(unique);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_at(home: &Path) -> Config {
        Config {
            home: home.to_path_buf(),
            board_url: DEFAULT_BOARD_URL.to_string(),
            inherited_path: None,
            board_profile: None,
        }
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(parse(&args(&["serve"])), Ok(Command::Serve));
        assert_eq!(
            parse(&args(&["pool", "rider"])),
            Ok(Command::Pool(project("rider")))
        );
        assert_eq!(
            parse(&args(&["pool", "my_app.v2-x"])),
            Ok(Command::Pool(project("my_app.v2-x")))
        );
        assert_eq!(
            parse(&args(&["run-task", "42"])),
            Ok(Command::RunTask(TaskId(42)))
        );
        assert_eq!(
            parse(&args(&["run-task", "#7"])),
            Ok(Command::RunTask(TaskId(7)))
        );
        assert_eq!(parse(&args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse(&args(&["-h"])), Ok(Command::Help));
        assert_eq!(parse(&args(&["help"])), Ok(Command::Help));
    }

    #[test]
    fn rejects_missing_extra_and_unknown_arguments() {
        assert_eq!(parse(&[]), Err(UsageError::NoCommand));
        assert_eq!(
            parse(&args(&["pool"])),
            Err(UsageError::MissingArgument("project"))
        );
        assert_eq!(
            parse(&args(&["run-task"])),
            Err(UsageError::MissingArgument("task id"))
        );
        assert_eq!(
            parse(&args(&["serve", "now"])),
            Err(UsageError::ExtraArgument("now".to_string()))
        );
        assert_eq!(
            parse(&args(&["pool", "a", "b"])),
            Err(UsageError::ExtraArgument("b".to_string()))
        );
        assert_eq!(
            parse(&args(&["run-task", "1", "2"])),
            Err(UsageError::ExtraArgument("2".to_string()))
        );
        assert_eq!(
            parse(&args(&["--help", "pool"])),
            Err(UsageError::ExtraArgument("pool".to_string()))
        );
        assert_eq!(
            parse(&args(&["start"])),
            Err(UsageError::UnknownCommand("start".to_string()))
        );
    }

    #[test]
    fn rejects_project_names_unsafe_in_a_channel() {
        let long = "a".repeat(PROJECT_LEN_MAX + 1);
        for bad in ["", "-x", ".x", "a/b", "a b", "ü", "work/x", long.as_str()] {
            assert_eq!(
                parse(&args(&["pool", bad])),
                Err(UsageError::BadProject(bad.to_string())),
                "{bad:?}"
            );
        }
        let longest = "a".repeat(PROJECT_LEN_MAX);
        assert_eq!(
            parse(&args(&["pool", &longest])),
            Ok(Command::Pool(project(&longest)))
        );
        assert_eq!(project("rider").channel(), "work/rider");
    }

    #[test]
    fn rejects_task_ids_that_are_not_decimal() {
        for bad in [
            "",
            "#",
            "-1",
            "+1",
            "1.0",
            "0x1",
            "##1",
            "18446744073709551616",
            "12a",
        ] {
            assert_eq!(
                parse(&args(&["run-task", bad])),
                Err(UsageError::BadTaskId(bad.to_string())),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse(&args(&["run-task", "18446744073709551615"])),
            Ok(Command::RunTask(TaskId(u64::MAX)))
        );
    }

    #[test]
    fn usage_errors_read_as_sentences() {
        assert_eq!(UsageError::NoCommand.to_string(), "no command given");
        assert_eq!(
            UsageError::BadProject("a/b".to_string()).to_string(),
            "\"a/b\" is not a project name: use 1 to 64 letters, digits, '-', '_' or '.', \
             not starting with '-' or '.'"
        );
        assert_eq!(
            UsageError::MissingArgument("project").to_string(),
            "the project is missing"
        );
    }

    #[test]
    fn config_defaults_to_home_and_the_local_board() {
        let vars = Vars {
            home: Some("/Users/someone".into()),
            path: Some("/usr/bin:/bin".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(vars),
            Ok(Config {
                home: PathBuf::from("/Users/someone/.rider"),
                board_url: "http://127.0.0.1:4100/api/board".to_string(),
                inherited_path: Some("/usr/bin:/bin".to_string()),
                board_profile: None,
            })
        );
        let vars = Vars {
            rider_home: Some("/srv/rider".into()),
            board_url: Some("http://127.0.0.1:5000/api/board".into()),
            board_profile: Some("review-1-sol-6-1".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(vars),
            Ok(Config {
                home: PathBuf::from("/srv/rider"),
                board_url: "http://127.0.0.1:5000/api/board".to_string(),
                inherited_path: None,
                board_profile: Some("review-1-sol-6-1".to_string()),
            })
        );
    }

    #[test]
    fn config_rejects_a_relative_missing_or_malformed_home() {
        assert_eq!(config(Vars::default()), Err(ConfigError::NoHome));
        let relative = Vars {
            rider_home: Some("rider".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(relative),
            Err(ConfigError::NotAbsolute {
                var: "RIDER_HOME",
                value: "rider".to_string()
            })
        );
        let empty_url = Vars {
            rider_home: Some("/srv/rider".into()),
            board_url: Some("".into()),
            ..Vars::default()
        };
        assert_eq!(config(empty_url), Err(ConfigError::Empty("BOARD_URL")));
        let empty_profile = Vars {
            rider_home: Some("/srv/rider".into()),
            board_profile: Some("".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(empty_profile),
            Err(ConfigError::Empty("BOARD_PROFILE"))
        );
    }

    #[test]
    fn config_rejects_variables_that_are_not_unicode() {
        use std::os::unix::ffi::OsStringExt;
        let bad = || Some(OsString::from_vec(vec![0x2f, 0xff]));
        let home = Vars {
            rider_home: bad(),
            ..Vars::default()
        };
        assert_eq!(config(home), Err(ConfigError::NotUnicode("RIDER_HOME")));
        let path = Vars {
            rider_home: Some("/srv/rider".into()),
            path: bad(),
            ..Vars::default()
        };
        assert_eq!(config(path), Err(ConfigError::NotUnicode("PATH")));
    }

    #[test]
    fn config_rejects_a_home_with_a_colon_and_a_malformed_profile() {
        let colon = Vars {
            rider_home: Some("/srv/a:b".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(colon),
            Err(ConfigError::HomeHasColon("/srv/a:b".to_string()))
        );
        for bad in [
            "a b",
            "line\nbreak",
            "x".repeat(PROFILE_LEN_MAX + 1).as_str(),
        ] {
            let vars = Vars {
                rider_home: Some("/srv/rider".into()),
                board_profile: Some(bad.into()),
                ..Vars::default()
            };
            assert_eq!(config(vars), Err(ConfigError::BadProfile(bad.to_string())));
        }
    }

    #[test]
    fn puts_vb_first_on_the_agents_path() {
        let cmd = CommandLine {
            program: "claude".to_string(),
            args: vec!["-p".to_string()],
            env: vec![("BOARD_TASK".to_string(), "7".to_string())],
        };
        let with = with_vb_on_path(&cmd, "/h/node/node_modules/.bin", Some("/usr/bin:/bin"));
        assert_eq!(
            with,
            CommandLine {
                program: "claude".to_string(),
                args: vec!["-p".to_string()],
                env: vec![
                    ("BOARD_TASK".to_string(), "7".to_string()),
                    (
                        "PATH".to_string(),
                        "/h/node/node_modules/.bin:/usr/bin:/bin".to_string()
                    ),
                ],
            }
        );
        let alone = with_vb_on_path(&cmd, "/h/node/node_modules/.bin", None);
        assert_eq!(
            alone.env[1],
            ("PATH".to_string(), "/h/node/node_modules/.bin".to_string())
        );
        let empty = with_vb_on_path(&cmd, "/h/bin", Some(""));
        assert_eq!(empty.env[1], ("PATH".to_string(), "/h/bin".to_string()));
    }

    #[test]
    fn a_path_the_command_already_sets_is_replaced() {
        let cmd = CommandLine {
            program: "codex".to_string(),
            args: Vec::new(),
            env: vec![("PATH".to_string(), "/old".to_string())],
        };
        let with = with_vb_on_path(&cmd, "/h/bin", Some("/usr/bin"));
        assert_eq!(
            with.env,
            vec![("PATH".to_string(), "/h/bin:/usr/bin".to_string())]
        );
    }

    #[test]
    fn stop_is_a_line_of_its_own() {
        assert!(is_stop("stop"));
        assert!(is_stop("  stop \r"));
        assert!(!is_stop("stop now"));
        assert!(!is_stop(""));
        assert!(!is_stop("STOP"));
    }

    #[test]
    fn every_role_has_an_embedded_prompt() {
        for (index, (name, _)) in PROMPTS.iter().enumerate() {
            assert!(
                *name == "slop-rules.md" || ROLES.iter().any(|role| role.prompt_file == *name),
                "{name} is unused"
            );
            assert!(!PROMPTS[..index].iter().any(|(earlier, _)| earlier == name));
        }
        for role in ROLES.iter() {
            let found = PROMPTS.iter().find(|(name, _)| *name == role.prompt_file);
            let (_, text) = found.unwrap_or_else(|| panic!("no prompt for {}", role.label));
            assert!(!text.trim().is_empty(), "{} is empty", role.prompt_file);
        }
    }

    #[test]
    fn writes_the_embedded_prompts_over_what_is_there() {
        let home = scratch("prompts");
        let dir = home.join("prompts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("review.md"), "stale").unwrap();
        write_prompts(&config_at(&home)).unwrap();
        for (name, text) in PROMPTS.iter() {
            assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), *text);
        }
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn serve_runs_vb_in_the_foreground_and_returns_its_exit_code() {
        let home = scratch("serve");
        let bin = home.join("node/node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        let record = home.join("args");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 3\n",
            record.display()
        );
        write_executable(&bin.join("vb"), &script);
        let code = execute(&Command::Serve, &config_at(&home)).unwrap();
        assert_eq!(code, 3);
        let board = home.join("board");
        assert!(board.is_dir());
        assert_eq!(
            fs::read_to_string(&record).unwrap(),
            format!("serve\n--dir\n{}\n--no-open\n", board.display())
        );
        assert!(!home.join("prompts").exists(), "serve copies no prompts");
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn run_task_needs_a_board_profile() {
        let home = scratch("run-task");
        let error = execute(&Command::RunTask(TaskId(1)), &config_at(&home)).unwrap_err();
        assert!(matches!(error, CliError::NoProfile), "{error}");
        fs::remove_dir_all(&home).unwrap();
    }
}
