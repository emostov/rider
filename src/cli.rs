//! The command line: parse the arguments, read the environment once, and run one command.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use fs4::{FileExt, TryLockError};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::board::{self, Board, BoardError, TaskId, VbCli};
use crate::config::{self, ConfigFileError, Limits};
use crate::daemon::{self, DaemonError, Paths};
use crate::feed::{
    BoardEvent, Cursor, EventKind, FeedError, FeedEvent, FeedItem, FeedReader, FeedStop,
};
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
/// The directory under the Rider home that holds the built-in worker prompts.
pub const PROMPTS_DIR: &str = "prompts";
const BOARD_DIR: &str = "board";
const VB_DIR: &str = "node/node_modules/.bin";
const ID_DIGITS_MAX: usize = 20;
/// Upper bound on the characters of a project or profile name.
const NAME_CHARS_MAX: usize = 64;
/// The exit code of a `rider wait` whose timeout passed with nothing to report.
pub const WAIT_TIMED_OUT: u8 = 3;
/// Upper bound on `rider wait --timeout`, in minutes.
const WAIT_MINUTES_MAX: u32 = 60;
const WAIT_MINUTES_DEFAULT: u32 = 54;
/// The directory under the Rider home that holds each project's wait cursor.
const WAIT_DIR: &str = "wait";
/// The profile `rider wait` reads the board as.
const WAIT_PROFILE: &str = "rider-wait";
/// The profile prefix of a lead, as the ride-swarm skill names it.
const LEAD_PREFIX: &str = "lead-";
/// The channel of the questions for the owner.
const DECISIONS: &str = "decisions";
/// How long the feed must stay quiet after an event before the wait reports it.
const WAIT_SETTLE: Duration = Duration::from_secs(2);
/// Upper bound on the events one wait reports; the rest wait for the next.
const WAIT_EVENTS_MAX: usize = 64;
/// Upper bound on the feed items one wait reads.
const WAIT_ITEMS_MAX: u64 = 1 << 20;
/// The feed items queued ahead of `rider wait`.
const WAIT_QUEUED_MAX: usize = 16;
/// Upper bound on the signals one wait handles.
const WAIT_SIGNALS_MAX: usize = 64;
/// Upper bound on the bytes of a wait cursor file.
const CURSOR_BYTES_MAX: u64 = 64;
/// The task states that leave a task open.
const OPEN_STATES: [&str; 4] = ["waiting", "ready", "claimed", "blocked"];

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
    Config,
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
    /// Block until something needs the lead, then print what happened since the last wait.
    ///
    /// Exits 3, printing nothing, when the timeout passes first.
    Wait {
        /// The project whose tasks are in work/<PROJECT>.
        #[arg(value_name = "PROJECT", value_parser = parse_name)]
        project: String,
        /// The most minutes to wait.
        #[arg(
            long,
            value_name = "MINUTES",
            default_value_t = WAIT_MINUTES_DEFAULT,
            value_parser = clap::value_parser!(u32).range(..=i64::from(WAIT_MINUTES_MAX)),
        )]
        timeout: u32,
        /// The lead's profile, whose own events never end the wait; by default any lead-*.
        #[arg(long = "as", value_name = "PROFILE", value_parser = parse_name)]
        lead: Option<String>,
    },
}

/// An argument value rider does not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    BadTaskId(String),
    EmptyUrl,
    BadName(String),
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
            UsageError::BadName(text) => write!(
                f,
                "{text:?} is not a name: use at most {NAME_CHARS_MAX} letters, digits, - or _"
            ),
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

fn parse_name(text: &str) -> Result<String, UsageError> {
    let named = !text.is_empty()
        && text.len() <= NAME_CHARS_MAX
        && !text.starts_with('-')
        && (text.bytes()).all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !named {
        return Err(UsageError::BadName(text.to_string()));
    }
    assert!(!text.contains('/') && !text.contains('.'));
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
    Board(BoardError),
    /// The board's feed ended or failed while `rider wait` followed it.
    Feed(String),
    /// The file holding `rider wait`'s cursor holds something else.
    BadCursor(PathBuf),
    /// Another `rider wait` holds the project's lock.
    Held {
        path: PathBuf,
        holder: String,
    },
    /// SIGTERM or SIGINT stopped `rider wait` before it had anything to print.
    Interrupted,
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
            CliError::Board(error) => write!(f, "{error}"),
            CliError::Feed(why) => write!(f, "the board's feed stopped: {why}"),
            CliError::Held { path, holder } => {
                write!(f, "{} is held by {holder}", path.display())
            }
            CliError::Interrupted => write!(f, "stopped by a signal"),
            CliError::BadCursor(path) => {
                write!(f, "{} does not hold a board cursor", path.display())
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
        Command::Serve => serve(config),
        Command::Daemon => daemon(config),
        Command::Up => up(config),
        Command::Down { now } => down(config, *now),
        Command::Status => status(config),
        Command::Agents => agents(config),
        Command::Logs { id, follow } => logs(config, *id, *follow),
        Command::Kill { id } => kill(config, *id),
        Command::Config => print_config(config),
        Command::Reap { urls, yes } => reap_leaks(urls, *yes, config),
        Command::Wait {
            project,
            timeout,
            lead,
        } => wait(config, project, *timeout, lead.as_deref()),
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
    let table = roles::install(settings.roles).map_err(|_| CliError::TableInstalled)?;
    assert!(!table.is_empty() && limits.agents > 0);
    Ok((table, limits))
}

fn print_config(config: &Config) -> Result<u8, CliError> {
    let settings = config::load(&config.home).map_err(CliError::ConfigFile)?;
    assert!(settings.toml.ends_with('\n'));
    let mut out = io::stdout().lock();
    out.write_all(settings.toml.as_bytes())
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

/// What happened to a task that ends a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Happened {
    Completed,
    Failed,
    Blocked,
    Answered,
}

/// What the wait follows: the project's channel, and whose events are the lead's own.
struct Watch<'a> {
    channel: String,
    lead: Option<&'a str>,
}

impl Watch<'_> {
    fn happened(&self, event: &BoardEvent) -> Option<Happened> {
        let profile = event
            .actor
            .as_deref()
            .and_then(|actor| actor.rsplit_once('+'));
        let by_lead = profile.is_some_and(|(_, profile)| match self.lead {
            Some(lead) => profile == lead,
            None => profile.starts_with(LEAD_PREFIX),
        });
        if by_lead {
            return None;
        }
        if event.channel == self.channel {
            return match event.kind {
                EventKind::TaskCompleted => Some(Happened::Completed),
                EventKind::TaskFailed => Some(Happened::Failed),
                EventKind::TaskBlocked => Some(Happened::Blocked),
                _ => None,
            };
        }
        let answer = matches!(
            event.kind,
            EventKind::MessagePosted | EventKind::TaskUnblocked | EventKind::TaskCompleted
        );
        (event.channel == DECISIONS && answer).then_some(Happened::Answered)
    }
}

fn wait(config: &Config, project: &str, minutes: u32, lead: Option<&str>) -> Result<u8, CliError> {
    assert!(minutes <= WAIT_MINUTES_MAX);
    let deadline = Instant::now() + Duration::from_secs(u64::from(minutes) * 60);
    let board = VbCli {
        vb: config.vb_dir().join("vb"),
        url: config.board_url.clone(),
        profile: WAIT_PROFILE.to_string(),
    };
    let watch = Watch {
        channel: format!("work/{project}"),
        lead,
    };
    let dir = config.home.join(WAIT_DIR);
    let _lock = lock_project(&dir, project)?;
    let (sender, items) = sync_channel(WAIT_QUEUED_MAX);
    start_signals(sender.clone())?;
    let path = dir.join(format!("{project}.cursor"));
    let saved = read_cursor(&path)?;
    let open = board.list_labelled(&watch.channel, "board.task", &OPEN_STATES);
    let open = open.map_err(CliError::Board)?;
    let start = saved.unwrap_or(open.cursor);
    let idle = open.tasks.is_empty() && open.left_out == 0;
    let until = if idle {
        Instant::now() + WAIT_SETTLE
    } else {
        deadline
    };
    let (happened, cursor) = follow(&board, &watch, start, until, (sender, items))?;
    let mut lines = describe(&board, &happened)?;
    if !happened.is_empty() {
        let open = board.list_labelled(&watch.channel, "board.task", &OPEN_STATES);
        let open = open.map_err(CliError::Board)?;
        if open.tasks.is_empty() && open.left_out == 0 {
            lines.push("idle".to_string());
        }
    } else if idle {
        lines.push("idle".to_string());
    }
    assert!(cursor >= start);
    let code = if lines.is_empty() {
        WAIT_TIMED_OUT
    } else {
        print_lines(&lines)?;
        0
    };
    write_cursor(&path, cursor)?;
    Ok(code)
}

fn print_lines(lines: &[String]) -> Result<(), CliError> {
    assert!(!lines.is_empty() && lines.len() <= WAIT_EVENTS_MAX + 1);
    let mut out = io::stdout().lock();
    let written = lines.iter().try_for_each(|line| writeln!(out, "{line}"));
    written
        .and_then(|()| out.flush())
        .map_err(|error| io_error("write", Path::new("standard output"), &error))
}

fn lock_project(dir: &Path, project: &str) -> Result<File, CliError> {
    fs::create_dir_all(dir).map_err(|error| io_error("create", dir, &error))?;
    let path = dir.join(format!("{project}.lock"));
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path);
    let mut file = opened.map_err(|error| io_error("open", &path, &error))?;
    match FileExt::try_lock(&file) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let read = (&file).take(CURSOR_BYTES_MAX).read_to_string(&mut text);
            read.map_err(|error| io_error("read", &path, &error))?;
            let holder = match board::one_line(&text) {
                line if line.is_empty() => "a process that has not written its pid".to_string(),
                line => line,
            };
            return Err(CliError::Held { path, holder });
        }
        Err(TryLockError::Error(error)) => return Err(io_error("lock", &path, &error)),
    }
    let text = format!("pid {}\n", std::process::id());
    let written = file
        .set_len(0)
        .and_then(|()| file.write_all(text.as_bytes()));
    written.map_err(|error| io_error("write", &path, &error))?;
    assert!(path.starts_with(dir));
    Ok(file)
}

fn start_signals(sender: SyncSender<Followed>) -> Result<(), CliError> {
    let signals = Signals::new([SIGTERM, SIGINT]);
    let mut signals = signals.map_err(|error| io_error("handle", Path::new("signals"), &error))?;
    let spawned = thread::Builder::new()
        .name("wait-signals".into())
        .spawn(move || {
            for _ in signals.forever().take(WAIT_SIGNALS_MAX) {
                if sender.send(Followed::Interrupted).is_err() {
                    return;
                }
            }
        });
    spawned.map_err(|error| CliError::Feed(error.to_string()))?;
    Ok(())
}

/// The feed as one thread reads it: first its stopper, then each item.
enum Followed {
    Started(FeedStop),
    Item(Result<FeedItem, FeedError>),
    Interrupted,
}

type Channel = (SyncSender<Followed>, Receiver<Followed>);

fn start_feed(
    board: &VbCli,
    start: Cursor,
    sender: SyncSender<Followed>,
    items: &Receiver<Followed>,
) -> Result<(JoinHandle<()>, FeedStop, bool), CliError> {
    let feed = VbCli {
        vb: board.vb.clone(),
        url: board.url.clone(),
        profile: board.profile.clone(),
    };
    let reader = thread::Builder::new()
        .name("wait-feed".into())
        .spawn(move || {
            let mut reader = match FeedReader::start(&feed, start) {
                Ok(reader) => reader,
                Err(error) => return drop(sender.send(Followed::Item(Err(error)))),
            };
            if sender.send(Followed::Started(reader.stopper())).is_err() {
                return;
            }
            for _ in 0..WAIT_ITEMS_MAX {
                let item = reader.next_item();
                let last = matches!(item, Ok(FeedItem::Exited { .. }) | Err(FeedError::Ended));
                if sender.send(Followed::Item(item)).is_err() || last {
                    return;
                }
            }
        });
    let reader = reader.map_err(|error| CliError::Feed(error.to_string()))?;
    let mut interrupted = false;
    for _ in 0..=WAIT_QUEUED_MAX {
        match items.recv() {
            Ok(Followed::Started(stop)) => return Ok((reader, stop, interrupted)),
            Ok(Followed::Interrupted) => interrupted = true,
            Ok(Followed::Item(Err(error))) => return Err(CliError::Feed(error.to_string())),
            Ok(Followed::Item(Ok(_))) | Err(_) => break,
        }
    }
    Err(CliError::Feed("the feed did not start".into()))
}

fn follow(
    board: &VbCli,
    watch: &Watch,
    start: Cursor,
    until: Instant,
    (sender, items): Channel,
) -> Result<(Vec<(Happened, TaskId)>, Cursor), CliError> {
    let (reader, stop, interrupted) = start_feed(board, start, sender, &items)?;
    let (mut happened, mut cursor, mut until) = (Vec::new(), start, until);
    let mut ended = Err(CliError::Feed(format!("over {WAIT_ITEMS_MAX} feed items")));
    if interrupted {
        ended = Err(CliError::Interrupted);
    }
    let rounds = if interrupted { 0 } else { WAIT_ITEMS_MAX };
    for _ in 0..rounds {
        let left = until.saturating_duration_since(Instant::now());
        let item = match items.recv_timeout(left) {
            Ok(Followed::Item(item)) => item,
            Ok(Followed::Started(_)) => Err(FeedError::Spawn("the feed started twice".into())),
            Ok(Followed::Interrupted) => {
                ended = Err(CliError::Interrupted);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                ended = Ok(());
                break;
            }
            Err(RecvTimeoutError::Disconnected) => Err(FeedError::Ended),
        };
        let event = match item {
            Ok(FeedItem::Event(FeedEvent::Known(event))) => event,
            Ok(FeedItem::Event(FeedEvent::Other { cursor: other })) => {
                cursor = cursor.max(other);
                continue;
            }
            Ok(FeedItem::Idle)
            | Err(FeedError::LineTooLong | FeedError::Parse(_) | FeedError::TooManyLabels) => {
                continue;
            }
            Ok(FeedItem::Exited { code }) => {
                ended = Err(CliError::Feed(format!("vb tail exited with {code:?}")));
                break;
            }
            Err(error) => {
                ended = Err(CliError::Feed(error.to_string()));
                break;
            }
        };
        let what = watch.happened(&event);
        if what.is_some() && happened.len() == WAIT_EVENTS_MAX {
            ended = Ok(());
            break;
        }
        cursor = cursor.max(event.cursor);
        if let Some(what) = what {
            if !happened.contains(&(what, event.task)) {
                happened.push((what, event.task));
            }
            until = Instant::now() + WAIT_SETTLE;
        }
    }
    let stopped = stop.stop();
    drop(items);
    let _ = reader.join();
    ended?;
    stopped.map_err(|error| CliError::Feed(error.to_string()))?;
    assert!(happened.len() <= WAIT_EVENTS_MAX && cursor >= start);
    Ok((happened, cursor))
}

fn describe(board: &VbCli, happened: &[(Happened, TaskId)]) -> Result<Vec<String>, CliError> {
    assert!(happened.len() <= WAIT_EVENTS_MAX);
    let lines: Vec<Result<String, BoardError>> = thread::scope(|scope| {
        let reads: Vec<_> = (happened.iter())
            .map(|&(what, id)| scope.spawn(move || describe_one(board, what, id)))
            .collect();
        let joined = reads.into_iter().map(|read| {
            let read = read.join();
            read.unwrap_or_else(|_| Err(BoardError::Io("a board read panicked".into())))
        });
        joined.collect()
    });
    let lines: Result<Vec<String>, BoardError> = lines.into_iter().collect();
    let lines = lines.map_err(CliError::Board)?;
    assert_eq!(lines.len(), happened.len());
    Ok(lines)
}

fn describe_one(board: &VbCli, what: Happened, id: TaskId) -> Result<String, BoardError> {
    let view = board.read_task(id)?;
    let first = board::one_line(view.text.lines().next().unwrap_or_default());
    let word = match what {
        Happened::Completed => "completed",
        Happened::Failed => "failed",
        Happened::Blocked => "blocked",
        Happened::Answered => "answered",
    };
    let review = (view.labels.iter()).any(|(key, value)| key == "role" && value == "review");
    let mut line = format!("{word} #{} {first}", id.0);
    if what == Happened::Completed && review {
        let blocking = board.blocking_findings(id)?;
        line.push_str(&format!(" ({blocking} blocking findings)"));
    }
    assert!(line.starts_with(word));
    Ok(line.trim_end().to_string())
}

fn read_cursor(path: &Path) -> Result<Option<Cursor>, CliError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("open", path, &error)),
    };
    let mut text = String::new();
    let read = file.take(CURSOR_BYTES_MAX).read_to_string(&mut text);
    read.map_err(|error| io_error("read", path, &error))?;
    let digits = text.strip_suffix('\n').unwrap_or(&text);
    let canonical = !digits.is_empty()
        && digits.len() <= ID_DIGITS_MAX
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    let cursor = digits.parse::<u64>().ok().filter(|_| canonical);
    let cursor = cursor.ok_or_else(|| CliError::BadCursor(path.to_path_buf()))?;
    assert_eq!(cursor.to_string(), digits);
    Ok(Some(Cursor(cursor)))
}

fn write_cursor(path: &Path, cursor: Cursor) -> Result<(), CliError> {
    let dir = path
        .parent()
        .ok_or_else(|| CliError::BadCursor(path.to_path_buf()))?;
    fs::create_dir_all(dir).map_err(|error| io_error("create", dir, &error))?;
    let partial = path.with_extension(format!("cursor.{}.partial", std::process::id()));
    let text = format!("{}\n", cursor.0);
    fs::write(&partial, &text).map_err(|error| io_error("write", &partial, &error))?;
    fs::rename(&partial, path).map_err(|error| io_error("rename", &partial, &error))?;
    assert!(u64::try_from(text.len()).is_ok_and(|len| len <= CURSOR_BYTES_MAX));
    Ok(())
}

fn io_error(action: &'static str, path: &Path, error: &io::Error) -> CliError {
    CliError::Io {
        action,
        path: path.to_path_buf(),
        message: error.to_string(),
    }
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
}
