//! The command line: parse the arguments, read the environment once, and run one command.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use crate::board::TaskId;
use crate::config::{self, ConfigFileError, Limits};
use crate::daemon::{self, DaemonError, Paths};
use crate::harness::{CommandLine, LOG_BYTES_MAX};
use crate::reap::{self, ReapError, Stopped};
use crate::roles::{self, RoleSpec};
use crate::socket::{self, Client, ClientError, SOCKET_FILE};

const ENVIRONMENT: &str = r#"environment:
  RIDER_HOME     rider's files, an absolute path (default $HOME/.rider)
  BOARD_URL      the board's API (default http://127.0.0.1:4100/api/board)"#;

/// Upper bound on the boards one `rider reap` names.
pub const REAP_URLS_MAX: usize = 8;
/// Upper bound on the arguments rider looks at, after the program name: the longest command.
pub const ARGS_MAX: usize = 2 * REAP_URLS_MAX + 2;
/// Every role's prompt file and the files they read, once each, built into the binary.
pub const PROMPTS: [(&str, &str); 4] = [
    ("implement.md", include_str!("../prompts/implement.md")),
    ("review.md", include_str!("../prompts/review.md")),
    ("security.md", include_str!("../prompts/security.md")),
    ("qa.md", include_str!("../prompts/qa.md")),
];
const DEFAULT_BOARD_URL: &str = "http://127.0.0.1:4100/api/board";
const HOME_DEFAULT_DIR: &str = ".rider";
/// The directory under the Rider home that holds the built-in worker prompts.
pub const PROMPTS_DIR: &str = "prompts";
const BOARD_DIR: &str = "board";
const VB_DIR: &str = "node/node_modules/.bin";
const ID_DIGITS_MAX: usize = 20;

/// The arguments rider was run with.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "rider", about = "Ride a swarm of coding agents", after_help = ENVIRONMENT)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// One command to run.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the local board in the foreground.
    Serve,
    /// Claim and run tasks on every work/ channel in the foreground.
    ///
    /// SIGTERM or Ctrl-C drains; a second one stops every agent now.
    Daemon,
    /// Start the daemon in the background and print its status line.
    Up,
    /// Stop the daemon once running agents finish; --now stops them at once.
    Down {
        /// Stop every agent now and release its task, its attempt uncounted.
        #[arg(long)]
        now: bool,
    },
    /// Print the board's state, the running count and every open task by project.
    Status,
    /// Print every agent the daemon runs.
    Agents,
    /// Print the newest attempt's log of a task; -f follows it until the agent exits.
    Logs {
        #[arg(value_name = "ID", value_parser = parse_task_id)]
        id: TaskId,
        /// Keep printing lines as the running agent writes them.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Stop a task's agent and fail the task; the attempt counts.
    Kill {
        #[arg(value_name = "ID", value_parser = parse_task_id)]
        id: TaskId,
    },
    /// Print the effective rider.toml: the built-in roles and limits with the file's over them.
    Config {
        /// Print the role table as this repository's tasks see it.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// List orphaned vb processes whose board no longer answers.
    ///
    /// Only those on the given boards if any; --yes stops them.
    Reap {
        /// A board whose vb processes to consider; repeat for more.
        #[arg(long = "url", value_name = "BOARD", value_parser = parse_url)]
        urls: Vec<String>,
        /// Stop the listed processes.
        #[arg(long)]
        yes: bool,
    },
}

/// An argument value rider does not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    BadTaskId(String),
    EmptyUrl,
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageError::BadTaskId(text) => {
                write!(
                    f,
                    "{text:?} is not a task id: use its number, such as 42 or #42"
                )
            }
            UsageError::EmptyUrl => write!(f, "a board URL is empty"),
        }
    }
}

impl std::error::Error for UsageError {}

fn parse_url(text: &str) -> Result<String, UsageError> {
    if text.is_empty() {
        return Err(UsageError::EmptyUrl);
    }
    assert!(!text.is_empty());
    Ok(text.to_string())
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
}

/// Reads every environment variable rider uses, once.
pub fn read_vars() -> Vars {
    Vars {
        rider_home: env::var_os("RIDER_HOME"),
        home: env::var_os("HOME"),
        board_url: env::var_os("BOARD_URL"),
        path: env::var_os("PATH"),
    }
}

/// Where rider's files are and how to reach the board, checked once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Absolute and UTF-8.
    pub home: PathBuf,
    pub board_url: String,
    pub inherited_path: Option<String>,
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
    let board_url = text(vars.board_url, "BOARD_URL")?;
    let config = Config {
        home,
        board_url: board_url.unwrap_or_else(|| DEFAULT_BOARD_URL.to_string()),
        inherited_path: text(vars.path, "PATH")?,
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

/// A command that could not finish.
#[derive(Debug)]
pub enum CliError {
    Config(ConfigError),
    Io {
        action: &'static str,
        path: PathBuf,
        message: String,
    },
    Daemon(DaemonError),
    Client(ClientError),
    /// `rider up` started a daemon that exited or never answered.
    NotStarted(String),
    Reap(ReapError),
    TooManyUrls,
    ConfigFile(ConfigFileError),
    TableInstalled,
    /// The task has no attempt log under the Rider home.
    NoLog(TaskId),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Config(error) => write!(f, "{error}"),
            CliError::Io {
                action,
                path,
                message,
            } => write!(f, "could not {action} {}: {message}", path.display()),
            CliError::Daemon(error) => write!(f, "{error}"),
            CliError::Client(error) => write!(f, "{error}"),
            CliError::NotStarted(why) => write!(f, "the daemon did not start: {why}"),
            CliError::Reap(error) => write!(f, "{error}"),
            CliError::TooManyUrls => write!(f, "name at most {REAP_URLS_MAX} boards"),
            CliError::ConfigFile(error) => write!(f, "{error}"),
            CliError::TableInstalled => write!(f, "the role table was already installed"),
            CliError::NoLog(id) => write!(f, "task #{} has no log", id.0),
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
        Command::Serve => serve(config),
        Command::Daemon => daemon(config),
        Command::Up => up(config),
        Command::Down { now } => down(config, *now),
        Command::Status => status(config),
        Command::Agents => agents(config),
        Command::Logs { id, follow } => logs(config, *id, *follow),
        Command::Kill { id } => kill(config, *id),
        Command::Config { repo } => print_config(config, repo.as_deref()),
        Command::Reap { urls, yes } => reap_leaks(urls, *yes, config),
    }
}

fn reap_leaks(urls: &[String], yes: bool, config: &Config) -> Result<u8, CliError> {
    if urls.len() > REAP_URLS_MAX {
        return Err(CliError::TooManyUrls);
    }
    let leaks = reap::find_leaks_on(&config.board_url, urls).map_err(CliError::Reap)?;
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

// Reads rider.toml and makes its role table the process's; returns the table and limits.
fn settings(config: &Config) -> Result<(&'static [RoleSpec], Limits), CliError> {
    let settings = config::load(&config.home).map_err(CliError::ConfigFile)?;
    let limits = settings.limits;
    let repos = settings.repos.into_iter().map(|repo| repo.roles).collect();
    let table = roles::install(settings.roles, repos).map_err(|_| CliError::TableInstalled)?;
    assert!(!table.is_empty() && limits.agents > 0);
    Ok((table, limits))
}

fn print_config(config: &Config, repo: Option<&Path>) -> Result<u8, CliError> {
    let settings = config::load(&config.home).map_err(CliError::ConfigFile)?;
    let canonical = repo.map(|repo| repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()));
    let toml = settings
        .repos
        .iter()
        .find(|entry| canonical.as_ref() == Some(&entry.roles.repo))
        .map_or(&settings.toml, |entry| &entry.toml);
    assert!(toml.ends_with('\n') && toml.starts_with("[limits]"));
    let mut out = io::stdout().lock();
    out.write_all(toml.as_bytes())
        .map_err(|error| CliError::Io {
            action: "write",
            path: PathBuf::from("standard output"),
            message: error.to_string(),
        })?;
    Ok(0)
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
    let vb = config.vb_dir().join("vb");
    let mut command = board_serve(&config.home, &vb).map_err(io("create", dir.clone()))?;
    // The board runs in the foreground, so it shares rider's terminal, standard input included.
    let status = command.status().map_err(io("run", vb))?;
    assert!(dir.is_absolute());
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

/// The `vb serve` command keeping the board of the Rider home `home`; makes its directory.
pub fn board_serve(home: &Path, vb: &Path) -> io::Result<Process> {
    assert!(home.is_absolute() && vb.is_absolute());
    let dir = home.join(BOARD_DIR);
    fs::create_dir_all(&dir)?;
    let mut command = Process::new(vb);
    command.arg("serve").arg("--dir").arg(&dir).arg("--no-open");
    assert!(dir.starts_with(home));
    Ok(command)
}

fn daemon(config: &Config) -> Result<u8, CliError> {
    let (table, limits) = settings(config)?;
    write_prompts(config)?;
    let paths = Paths {
        home: config.home.clone(),
        prompts: config.prompts(),
        vb: config.vb_dir().join("vb"),
        board_url: config.board_url.clone(),
        inherited_path: config.inherited_path.clone(),
    };
    assert!(paths.vb.is_absolute() && paths.prompts.is_absolute());
    daemon::run(paths, table, limits).map_err(CliError::Daemon)
}

/// How long `rider up` waits for the daemon's socket to answer.
const UP_SECONDS_MAX: u64 = 30;
/// How often `rider up` asks the socket again.
const UP_POLL_MILLIS: u64 = 100;
/// Upper bound on the lines one `rider down` prints.
const DOWN_LINES_MAX: usize = 4096;
/// Upper bound on the lines `rider logs -f` prints.
const LOG_LINES_MAX: u64 = u64::MAX;

fn up(config: &Config) -> Result<u8, CliError> {
    if let Ok(mut client) = Client::connect(&config.home) {
        let data = client.send("status", &[]).map_err(CliError::Client)?;
        println!("rider: the daemon is already running");
        println!("{}", status_line(&data));
        return Ok(0);
    }
    fs::create_dir_all(&config.home).map_err(|error| CliError::Io {
        action: "create",
        path: config.home.clone(),
        message: error.to_string(),
    })?;
    let log_path = config.home.join(daemon::LOG_FILE);
    let io = |action: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |error: io::Error| CliError::Io {
            action,
            path,
            message: error.to_string(),
        }
    };
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);
    let log = log.map_err(io("open", &log_path))?;
    let err = log.try_clone().map_err(io("open", &log_path))?;
    let exe = env::current_exe().map_err(io("find", Path::new("rider")))?;
    let mut child = Process::new(&exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(io("start", &exe))?;
    let deadline = Instant::now() + Duration::from_secs(UP_SECONDS_MAX);
    let polls = UP_SECONDS_MAX * 1000 / UP_POLL_MILLIS;
    for _ in 0..polls {
        if let Ok(mut client) = Client::connect(&config.home) {
            let data = client.send("status", &[]).map_err(CliError::Client)?;
            println!("{}", status_line(&data));
            return Ok(0);
        }
        if let Ok(Some(exit)) = child.try_wait() {
            let why = format!("it exited with {exit}; see {}", log_path.display());
            return Err(CliError::NotStarted(why));
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(UP_POLL_MILLIS));
    }
    let socket = config.home.join(SOCKET_FILE);
    let why = format!("{} did not answer in {UP_SECONDS_MAX} s", socket.display());
    Err(CliError::NotStarted(why))
}

fn down(config: &Config, now: bool) -> Result<u8, CliError> {
    let mut client = Client::connect(&config.home).map_err(CliError::Client)?;
    client
        .send("down", &[("now", now.into())])
        .map_err(CliError::Client)?;
    for _ in 0..DOWN_LINES_MAX {
        match client.next_line().map_err(CliError::Client)? {
            Some(line) => say(&line),
            None => return Ok(0),
        }
    }
    Err(CliError::Client(ClientError::Malformed(format!(
        "more than {DOWN_LINES_MAX} lines"
    ))))
}

fn logs(config: &Config, id: TaskId, follow: bool) -> Result<u8, CliError> {
    if follow {
        match Client::connect(&config.home) {
            Ok(mut client) => {
                let fields = [("task", id.0.into()), ("follow", true.into())];
                client.send("logs", &fields).map_err(CliError::Client)?;
                for _ in 0..LOG_LINES_MAX {
                    match client.next_line().map_err(CliError::Client)? {
                        // A closed standard output ends nothing the daemon does.
                        Some(line) => drop(writeln!(io::stdout().lock(), "{line}")),
                        None => return Ok(0),
                    }
                }
                let more = format!("more than {LOG_LINES_MAX} lines");
                return Err(CliError::Client(ClientError::Malformed(more)));
            }
            // With no daemon nothing writes the log any more, so it is printed as it is.
            Err(ClientError::NotRunning(_)) => {}
            Err(error) => return Err(CliError::Client(error)),
        }
    }
    let path = socket::newest_log(&config.home.join(daemon::LOGS_DIR), id);
    let path = path.ok_or(CliError::NoLog(id))?;
    let io = |error: io::Error| CliError::Io {
        action: "read",
        path: path.clone(),
        message: error.to_string(),
    };
    let file = fs::File::open(&path).map_err(io)?;
    let mut stdout = io::stdout().lock();
    io::copy(&mut file.take(LOG_BYTES_MAX), &mut stdout).map_err(io)?;
    stdout.flush().map_err(io)?;
    Ok(0)
}

fn kill(config: &Config, id: TaskId) -> Result<u8, CliError> {
    let mut client = Client::connect(&config.home).map_err(CliError::Client)?;
    let fields = [("task", id.0.into())];
    client.send("kill", &fields).map_err(CliError::Client)?;
    say(&format!("killed the agent of #{}", id.0));
    Ok(0)
}

fn status(config: &Config) -> Result<u8, CliError> {
    let mut client = Client::connect(&config.home).map_err(CliError::Client)?;
    let data = client.send("status", &[]).map_err(CliError::Client)?;
    println!("{}", status_line(&data));
    let tasks = data["tasks"].as_array().map_or(&[][..], Vec::as_slice);
    let mut projects: Vec<&str> = tasks
        .iter()
        .map(|task| task["project"].as_str().unwrap_or(daemon::SUBTREE))
        .collect();
    projects.sort_unstable();
    projects.dedup();
    for project in projects {
        println!("{project}");
        let in_project = tasks
            .iter()
            .filter(|task| task["project"].as_str().unwrap_or(daemon::SUBTREE) == project);
        for task in in_project {
            println!(
                "  #{} {} {} {} {} {}",
                task["id"],
                text(&task["state"]),
                text(&task["role"]),
                text(&task["profile"]),
                age(&task["age_ms"]),
                text(&task["branch"]),
            );
        }
    }
    Ok(0)
}

fn agents(config: &Config) -> Result<u8, CliError> {
    let mut client = Client::connect(&config.home).map_err(CliError::Client)?;
    let data = client.send("agents", &[]).map_err(CliError::Client)?;
    let rows = data.as_array().map_or(&[][..], Vec::as_slice);
    if rows.is_empty() {
        println!("no agents");
    }
    for row in rows {
        let output = match row["output_ms_ago"].as_u64() {
            Some(_) => format!("output {} ago", age(&row["output_ms_ago"])),
            None => "no output".to_string(),
        };
        println!(
            "#{} {} {} {} {} {output}",
            row["task"],
            text(&row["role"]),
            text(&row["profile"]),
            text(&row["phase"]),
            age(&row["age_ms"]),
        );
    }
    Ok(0)
}

// The first line of `status`: the board, the running count and limit, and any stop.
fn status_line(data: &serde_json::Value) -> String {
    let mut line = format!(
        "board {}, {} of {} running",
        text(&data["board"]),
        data["running"],
        data["limit"]
    );
    match data["stopping"].as_str() {
        Some("drain") => line.push_str(", draining"),
        Some("now") => line.push_str(", stopping now"),
        _ => {}
    }
    if let Some(count) = data["untracked"].as_u64().filter(|count| *count > 0) {
        let floor = if data["untracked_at_least"] == true {
            "at least "
        } else {
            ""
        };
        line.push_str(&format!(", {floor}{count} more tasks not tracked"));
    }
    line
}

fn text(value: &serde_json::Value) -> &str {
    value.as_str().unwrap_or("-")
}

fn age(value: &serde_json::Value) -> String {
    match value.as_u64() {
        None => "-".to_string(),
        Some(millis) if millis < 60_000 => format!("{}s", millis / 1000),
        Some(millis) if millis < 3_600_000 => format!("{}m", millis / 60_000),
        Some(millis) => format!("{}h", millis / 3_600_000),
    }
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

    fn parse(words: &[&str]) -> Result<Command, clap::Error> {
        let args = ["rider"].iter().chain(words);
        Cli::try_parse_from(args).map(|cli| cli.command)
    }

    fn usage_error(words: &[&str]) -> clap::error::ErrorKind {
        let error = parse(words).expect_err("a usage error");
        assert_eq!(error.exit_code(), 2, "{words:?}");
        error.kind()
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
        }
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(parse(&["serve"]).unwrap(), Command::Serve);
        assert_eq!(parse(&["daemon"]).unwrap(), Command::Daemon);
        let kill = |id| Command::Kill { id: TaskId(id) };
        assert_eq!(parse(&["kill", "42"]).unwrap(), kill(42));
        assert_eq!(parse(&["kill", "#7"]).unwrap(), kill(7));
        for help in [&["--help"][..], &["-h"], &["help"]] {
            assert_eq!(usage_help(help), clap::error::ErrorKind::DisplayHelp);
        }
    }

    fn usage_help(words: &[&str]) -> clap::error::ErrorKind {
        let error = parse(words).expect_err("help");
        assert_eq!(error.exit_code(), 0, "{words:?}");
        error.kind()
    }

    #[test]
    fn rejects_missing_extra_and_unknown_arguments() {
        use clap::error::ErrorKind;
        assert_eq!(
            usage_error(&[]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        for missing in [&["kill"][..], &["logs"]] {
            assert_eq!(usage_error(missing), ErrorKind::MissingRequiredArgument);
        }
        for extra in [&["serve", "now"][..], &["kill", "1", "2"]] {
            assert_eq!(usage_error(extra), ErrorKind::UnknownArgument);
        }
        for gone in [&["start"][..], &["pool", "rider"]] {
            assert_eq!(usage_error(gone), ErrorKind::InvalidSubcommand);
        }
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
                usage_error(&["kill", "--", bad]),
                clap::error::ErrorKind::ValueValidation,
                "{bad:?}"
            );
        }
        assert_eq!(
            parse(&["kill", "18446744073709551615"]).unwrap(),
            Command::Kill {
                id: TaskId(u64::MAX)
            }
        );
    }

    #[test]
    fn usage_errors_read_as_sentences() {
        assert_eq!(
            UsageError::BadTaskId("x".to_string()).to_string(),
            "\"x\" is not a task id: use its number, such as 42 or #42"
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
            })
        );
        let vars = Vars {
            rider_home: Some("/srv/rider".into()),
            board_url: Some("http://127.0.0.1:5000/api/board".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(vars),
            Ok(Config {
                home: PathBuf::from("/srv/rider"),
                board_url: "http://127.0.0.1:5000/api/board".to_string(),
                inherited_path: None,
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
    fn config_rejects_a_home_with_a_colon() {
        let colon = Vars {
            rider_home: Some("/srv/a:b".into()),
            ..Vars::default()
        };
        assert_eq!(
            config(colon),
            Err(ConfigError::HomeHasColon("/srv/a:b".to_string()))
        );
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
    fn every_role_has_an_embedded_prompt() {
        for (index, (name, _)) in PROMPTS.iter().enumerate() {
            assert!(
                ROLES.iter().any(|role| role.prompt_file == *name),
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
}
