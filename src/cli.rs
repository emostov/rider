//! The command line: parse the arguments, read the environment once, and run one command.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use fs4::{FileExt, TryLockError};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Deserialize;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::flag;
use signal_hook::iterator::Signals;

use crate::board::{
    self, Board, BoardError, Listing, NEEDS_MAX, TaskId, TaskState, TaskView, VbCli,
};
use crate::config::{self, ConfigFileError, Limits};
use crate::daemon::{self, DaemonError, Paths};
use crate::feed::{
    BoardEvent, Cursor, EventKind, FeedError, FeedEvent, FeedItem, FeedReader, FeedStop,
};
use crate::git::{self, GitError};
use crate::harness::LOG_BYTES_MAX;
use crate::queue::{self, QUEUE_CHANNEL, QUEUE_ITEMS_MAX, QueueError, STAND_IN_FILE};
use crate::reap::{self, ReapError, Stopped};
use crate::roles::{self, RoleSpec};
use crate::run_task::{RESULT_FILE_BYTES_MAX, result_path};
use crate::skills::{self, InstallDir, Outcome, Report, SKILLS, SkillsError};
use crate::socket::{self, Client, ClientError, SocketPath};

const ENVIRONMENT: &str = r#"environment:
  RIDER_HOME     rider's files, an absolute path (default $HOME/.rider)
  BOARD_URL      the board's API (default http://127.0.0.1:4100/api/board)"#;

/// Upper bound on the boards one `rider reap` names.
pub const REAP_URLS_MAX: usize = 8;
/// Upper bound on the arguments `rider vb` passes on: a full needs list with its words and options.
pub const VB_ARGS_MAX: usize = NEEDS_MAX + 64;
/// Upper bound on the arguments rider looks at, after the program name: the longest command.
pub const ARGS_MAX: usize = VB_ARGS_MAX + 1;
const _: () = assert!(ARGS_MAX >= 2 * REAP_URLS_MAX + 2);
pub const IMPLEMENT_PROMPT: &str = "implement.md";
pub const SLOP_RULES: &str = include_str!("../prompts/slop-rules.md");
/// Every role's prompt and the slop rules, once each, built into the binary.
pub const PROMPTS: [(&str, &str); 6] = [
    (IMPLEMENT_PROMPT, include_str!("../prompts/implement.md")),
    ("review.md", include_str!("../prompts/review.md")),
    (
        "native-review.md",
        include_str!("../prompts/native-review.md"),
    ),
    ("security.md", include_str!("../prompts/security.md")),
    ("slop-rules.md", SLOP_RULES),
    ("qa.md", include_str!("../prompts/qa.md")),
];
const DEFAULT_BOARD_URL: &str = "http://127.0.0.1:4100/api/board";
const HOME_DEFAULT_DIR: &str = ".rider";
const BOARD_DIR: &str = "board";
const NODE_DIR: &str = "node";
/// The Board CLI rider installs under its home when it finds none there, every package locked.
const BOARD_PACKAGE: [(&str, &str); 2] = [
    ("package.json", include_str!("../board-cli/package.json")),
    (
        "package-lock.json",
        include_str!("../board-cli/package-lock.json"),
    ),
];
const NPM_FLAGS: [&str; 5] = [
    "--ignore-scripts",
    "--no-global",
    "--no-audit",
    "--no-fund",
    "--loglevel=error",
];
/// The only variables npm gets from rider's environment.
const NPM_VARS: [&str; 2] = ["PATH", "HOME"];
const INSTALL_SECONDS_MAX: u64 = 600;
const INSTALL_SIGNALS: [i32; 4] = [SIGTERM, SIGINT, SIGHUP, SIGQUIT];
const INSTALL_LOCK: &str = "node.lock";
const INSTALL_POLL_MILLIS: u64 = 100;
/// Upper bound on the bytes of npm's error output that rider shows.
const INSTALL_OUTPUT_BYTES_MAX: u64 = 64 * 1024;
const SKILLS_DIR: &str = "skills";
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
/// The failure reason of a read of a channel that does not exist yet.
const CHANNEL_NOT_FOUND: &str = "channel_not_found";
/// The profile `rider queue` reads the board as.
const QUEUE_PROFILE: &str = "rider-queue";
/// The profile `rider wait` reads the board as.
const WAIT_PROFILE: &str = "rider-wait";
const LEAD_PROFILE: &str = "lead";
const LEAD_PREFIX: &str = "lead-";
const PR_PROFILE: &str = "rider-pr";
const PR_TITLE_CHARS_MAX: usize = 200;
const PR_LIST_MAX: usize = 20;
const BASE_CHAIN_MAX: usize = 16;
const PR_FIELDS: &str = "--json=url,headRefName,headRepository,headRepositoryOwner";
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
    /// Print the owner's review queue, then the work the daemon runs.
    Queue {
        /// Print the items with every label as JSON.
        #[arg(long)]
        json: bool,
    },
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
    /// Push a completed task's branch to origin and open its draft PR; print the PR's URL.
    ///
    /// Never forces: a branch that moved on origin is refused. An open PR is reused.
    Pr {
        #[arg(value_name = "ID", value_parser = parse_task_id)]
        id: TaskId,
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
        /// The lead's profile, whose own events never end the wait.
        #[arg(long = "as", value_name = "PROFILE", value_parser = parse_name)]
        lead: Option<String>,
    },
    /// Run the Board CLI on rider's board, passing every argument on as given.
    ///
    /// BOARD_URL is set to rider's board unless it is already set.
    #[command(disable_help_flag = true)]
    Vb {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<OsString>,
    },
    /// List, print or install the skills built into rider.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
}

/// What to do with the built-in skills.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SkillsCommand {
    /// Print each skill's name and description.
    List,
    /// Print a skill's SKILL.md.
    Show {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Write each skill as DIR/<skill>/ and update every recorded older install.
    Install {
        /// The agent's skills directory (default <rider home>/skills).
        #[arg(long, value_name = "DIR")]
        into: Option<PathBuf>,
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
}

/// Reads every environment variable rider uses, once.
pub fn read_vars() -> Vars {
    Vars {
        rider_home: env::var_os("RIDER_HOME"),
        home: env::var_os("HOME"),
        board_url: env::var_os("BOARD_URL"),
    }
}

/// Where rider's files are and how to reach the board, checked once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    // Absolute and UTF-8.
    home: PathBuf,
    board_url: String,
}

impl Config {
    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn board_url(&self) -> &str {
        &self.board_url
    }

    fn vb_dir(&self) -> PathBuf {
        self.home.join(NODE_DIR).join("node_modules/.bin")
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
    let text = |value: OsString, var: &'static str| match value.into_string() {
        Ok(text) if text.is_empty() => Err(ConfigError::Empty(var)),
        Ok(text) => Ok(text),
        Err(_) => Err(ConfigError::NotUnicode(var)),
    };
    let (home, var, value) = match (vars.rider_home, vars.home) {
        (Some(rider_home), _) => {
            let value = text(rider_home, "RIDER_HOME")?;
            (PathBuf::from(&value), "RIDER_HOME", value)
        }
        (None, Some(home)) => {
            let value = text(home, "HOME")?;
            (Path::new(&value).join(HOME_DEFAULT_DIR), "HOME", value)
        }
        (None, None) => return Err(ConfigError::NoHome),
    };
    if !home.is_absolute() {
        return Err(ConfigError::NotAbsolute { var, value });
    }
    if value.contains(':') {
        return Err(ConfigError::HomeHasColon(value));
    }
    let board_url = match vars.board_url {
        Some(url) => text(url, "BOARD_URL")?,
        None => DEFAULT_BOARD_URL.to_string(),
    };
    let config = Config { home, board_url };
    assert!(config.home.is_absolute() && config.home.to_str().is_some());
    assert!(!config.board_url.is_empty());
    Ok(config)
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
    NotPushable {
        id: TaskId,
        why: &'static str,
    },
    Git(GitError),
    BadGh(String),
    /// The Board CLI is missing and there is no `npm` to install it with.
    NoNpm,
    /// `npm install` of the Board CLI failed or left no `vb`.
    NotInstalled(String),
    Queue(QueueError),
    Skills(SkillsError),
    /// `rider skills install` left these skill directories as they were.
    SkillsRefused(usize),
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
            CliError::NotPushable { id, why } => write!(f, "task #{} {why}", id.0),
            CliError::Git(error) => write!(f, "{error}"),
            CliError::BadGh(output) => {
                write!(
                    f,
                    "gh printed output rider cannot use: {}",
                    board::one_line(output)
                )
            }
            CliError::NoNpm => write!(
                f,
                "the Board CLI is missing and npm is not on PATH: install Node 22, then retry"
            ),
            CliError::NotInstalled(why) => write!(f, "could not install the Board CLI: {why}"),
            CliError::Queue(error) => write!(f, "{error}"),
            CliError::Skills(error) => write!(f, "{error}"),
            CliError::SkillsRefused(count) => {
                write!(f, "{count} skills were not installed; see the lines above")
            }
            CliError::BadCursor(path) => {
                write!(f, "{} does not hold a board cursor", path.display())
            }
        }
    }
}

impl From<SkillsError> for CliError {
    fn from(error: SkillsError) -> CliError {
        CliError::Skills(error)
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> CliError {
        CliError::Config(error)
    }
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
        Command::Queue { json } => queue(config, *json),
        Command::Logs { id, follow } => logs(config, *id, *follow),
        Command::Kill { id } => kill(config, *id),
        Command::Config { repo } => print_config(config, repo.as_deref()),
        Command::Reap { urls, yes } => reap_leaks(urls, *yes, config),
        Command::Pr { id } => pr(config, *id),
        Command::Wait {
            project,
            timeout,
            lead,
        } => wait(config, project, *timeout, lead.as_deref()),
        Command::Vb { args } => vb(config, args),
        Command::Skills { command } => run_skills(config, command),
    }
}

fn run_skills(config: &Config, command: &SkillsCommand) -> Result<u8, CliError> {
    match command {
        SkillsCommand::List => {
            for name in SKILLS {
                say(&format!("{name}  {}", skills::description(name)?));
            }
        }
        SkillsCommand::Show { name } => {
            let text = skills::show(name)?;
            let mut out = io::stdout().lock();
            out.write_all(text.as_bytes())
                .map_err(|error| CliError::Io {
                    action: "write",
                    path: PathBuf::from("standard output"),
                    message: error.to_string(),
                })?;
        }
        SkillsCommand::Install { into } => {
            let dir = match into {
                Some(into) => skills::resolve(into)?,
                None => InstallDir::parse(&config.home.join(SKILLS_DIR))?,
            };
            let (report, others) = skills::install(&config.home, &dir)?;
            let mut refused = 0_usize;
            for (path, outcome) in &report {
                say(&skill_line(path, outcome));
                if matches!(outcome, Ok(Outcome::Edited(_) | Outcome::Foreign) | Err(_)) {
                    refused += 1;
                }
            }
            say_updates(&others);
            if refused > 0 {
                return Err(CliError::SkillsRefused(refused));
            }
        }
    }
    Ok(0)
}

fn skill_line(path: &Path, outcome: &Result<Outcome, SkillsError>) -> String {
    let path = path.display();
    match outcome {
        Ok(Outcome::Installed) => format!("installed {path}"),
        Ok(Outcome::UpToDate) => format!("up to date {path}"),
        Ok(Outcome::Replaced { from }) => format!("replaced {path} (was {from})"),
        Ok(Outcome::Edited(files)) => {
            format!("skipped {path}: edited by hand: {}", files.join(", "))
        }
        Ok(Outcome::Foreign) => format!("skipped {path}: not installed by rider"),
        Ok(Outcome::Newer(version)) => {
            format!(
                "kept {path}: rider {version} installed it, newer than {}",
                skills::VERSION
            )
        }
        Ok(Outcome::Absent) => format!("skipped {path}: removed"),
        Err(error) => format!("skipped {path}: {error}"),
    }
}

fn update_skills(config: &Config) {
    match skills::update_recorded(&config.home) {
        Ok(report) => say_updates(&report),
        Err(error) => say(&format!("rider: the skills were not updated: {error}")),
    }
}

fn say_updates(report: &Report) {
    for (path, outcome) in report {
        if !matches!(outcome, Ok(Outcome::UpToDate | Outcome::Absent)) {
            say(&skill_line(path, outcome));
        }
    }
}

fn vb(config: &Config, args: &[OsString]) -> Result<u8, CliError> {
    assert!(args.len() <= VB_ARGS_MAX);
    let vb = install_vb(config)?;
    assert!(vb.is_absolute() && !config.board_url.is_empty());
    let error = Process::new(&vb)
        .args(args)
        .env("BOARD_URL", &config.board_url)
        .exec();
    Err(io_error("run", &vb, &error))
}

fn install_vb(config: &Config) -> Result<PathBuf, CliError> {
    let vb = config.vb_dir().join("vb");
    if vb.is_file() {
        return Ok(vb);
    }
    let prefix = config.home.join(NODE_DIR);
    assert!(prefix.is_absolute() && vb.starts_with(&prefix));
    fs::create_dir_all(&prefix).map_err(|error| io_error("create", &prefix, &error))?;
    let lock_path = config.home.join(INSTALL_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path);
    let lock = lock.map_err(|error| io_error("open", &lock_path, &error))?;
    lock.lock()
        .map_err(|error| io_error("lock", &lock_path, &error))?;
    if vb.is_file() {
        return Ok(vb);
    }
    for (name, text) in BOARD_PACKAGE {
        let path = prefix.join(name);
        fs::write(&path, text).map_err(|error| io_error("write", &path, &error))?;
    }
    eprintln!("rider: installing the Board CLI into {}", prefix.display());
    let mut npm = Process::new("npm");
    npm.env_clear()
        .args(["ci", "--prefix"])
        .arg(&prefix)
        .args(NPM_FLAGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for var in NPM_VARS {
        if let Some(value) = env::var_os(var) {
            npm.env(var, value);
        }
    }
    let handlers = InstallSignals::register()?;
    let spawned = CommandWrap::from(npm).wrap(ProcessGroup::leader()).spawn();
    let child = match spawned {
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(CliError::NoNpm),
        Err(error) => return Err(io_error("run", Path::new("npm"), &error)),
        Ok(child) => child,
    };
    let (status, shown) = supervise_npm(child, &handlers.stopped)?;
    drop(handlers);
    if !status.success() {
        let text: String = String::from_utf8_lossy(&shown)
            .chars()
            .map(|c| {
                if c.is_control() && !matches!(c, '\n' | '\t') {
                    '?'
                } else {
                    c
                }
            })
            .collect();
        let _ = io::stderr().write_all(text.as_bytes());
        return Err(CliError::NotInstalled(format!("npm {status}")));
    }
    if !vb.is_file() {
        let why = format!("npm left no {}", vb.display());
        return Err(CliError::NotInstalled(why));
    }
    Ok(vb)
}

struct InstallSignals {
    stopped: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl InstallSignals {
    fn register() -> Result<InstallSignals, CliError> {
        let handlers = InstallSignals {
            stopped: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
        };
        let signals = Path::new("signals");
        for signal in INSTALL_SIGNALS {
            flag::register_conditional_default(signal, Arc::clone(&handlers.done))
                .and_then(|_| flag::register(signal, Arc::clone(&handlers.stopped)))
                .map_err(|error| io_error("handle", signals, &error))?;
        }
        assert!(!handlers.done.load(Ordering::SeqCst));
        Ok(handlers)
    }
}

impl Drop for InstallSignals {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

fn supervise_npm(
    mut child: Box<dyn ChildWrapper>,
    stopped: &AtomicBool,
) -> Result<(ExitStatus, Vec<u8>), CliError> {
    let npm = Path::new("npm");
    let deadline = Instant::now() + Duration::from_secs(INSTALL_SECONDS_MAX);
    let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let group = group.ok_or_else(|| CliError::NotInstalled("npm has no process id".into()))?;
    let stderr = child.stderr().take();
    let (sender, shown) = sync_channel(1);
    let reader = thread::Builder::new()
        .name("npm-stderr".into())
        .spawn(move || {
            let mut text = Vec::new();
            let Some(mut stderr) = stderr else {
                let _ = sender.send(text);
                return;
            };
            let _ = (&mut stderr)
                .take(INSTALL_OUTPUT_BYTES_MAX)
                .read_to_end(&mut text);
            let _ = sender.send(text);
            let _ = io::copy(&mut stderr, &mut io::sink());
        });
    reader.map_err(|error| io_error("read", npm, &error))?;
    let polls = INSTALL_SECONDS_MAX * 1000 / INSTALL_POLL_MILLIS;
    let mut ended = Err(CliError::NotInstalled(format!(
        "npm ran longer than {INSTALL_SECONDS_MAX} s"
    )));
    for _ in 0..polls {
        match child.try_wait() {
            Ok(Some(status)) => {
                ended = Ok(status);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                ended = Err(io_error("wait for", npm, &error));
                break;
            }
        }
        if stopped.load(Ordering::SeqCst) {
            ended = Err(CliError::Interrupted);
            break;
        }
        thread::sleep(Duration::from_millis(INSTALL_POLL_MILLIS));
    }
    match kill_process_group(group, Signal::KILL) {
        // Darwin refuses (EPERM) to signal a group whose only member is the exited leader.
        Ok(()) | Err(Errno::SRCH | Errno::PERM) => {}
        Err(error) => return Err(io_error("stop", npm, &error.into())),
    }
    if ended.is_err() {
        child
            .wait()
            .map_err(|error| io_error("stop", npm, &error))?;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    let shown = shown.recv_timeout(left.max(Duration::from_secs(1)));
    let shown = shown.map_err(|_| CliError::NotInstalled("npm's output did not end".into()))?;
    assert!(u64::try_from(shown.len()).is_ok_and(|len| len <= INSTALL_OUTPUT_BYTES_MAX));
    ended.map(|status| (status, shown))
}

fn reap_leaks(urls: &[String], yes: bool, config: &Config) -> Result<u8, CliError> {
    if urls.len() > REAP_URLS_MAX {
        return Err(CliError::TooManyUrls);
    }
    let leaks = reap::find_leaks_on(config.board_url(), urls).map_err(CliError::Reap)?;
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
    let vb = install_vb(config)?;
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
    let socket = SocketPath::under(&config.home).map_err(CliError::Client)?;
    let (table, limits) = settings(config)?;
    update_skills(config);
    let paths = Paths {
        home: config.home.clone(),
        socket,
        vb: config.vb_dir().join("vb"),
        board_url: config.board_url.clone(),
    };
    assert!(paths.vb.is_absolute() && paths.home.is_absolute());
    daemon::run(paths, table, limits).map_err(CliError::Daemon)
}

/// How long `rider up` waits for the daemon's socket to answer.
const UP_SECONDS_MAX: u64 = 30;
const UP_LOG_TAIL_BYTES: usize = 4096;
/// How often `rider up` asks the socket again.
const UP_POLL_MILLIS: u64 = 100;
/// Upper bound on the lines one `rider down` prints.
const DOWN_LINES_MAX: usize = 4096;
/// Upper bound on the lines `rider logs -f` prints.
const LOG_LINES_MAX: u64 = u64::MAX;

fn up(config: &Config) -> Result<u8, CliError> {
    let socket = SocketPath::under(&config.home).map_err(CliError::Client)?;
    update_skills(config);
    if let Ok(mut client) = Client::connect(&config.home) {
        let data = client.send("status", &[]).map_err(CliError::Client)?;
        println!("rider: the daemon is already running");
        println!("{}", status_line(&data));
        return Ok(0);
    }
    install_vb(config)?;
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
            let data = match client.send("status", &[]) {
                Ok(data) => data,
                Err(error) => return Err(not_started(&mut child, &log_path, deadline, error)),
            };
            println!("{}", status_line(&data));
            return Ok(0);
        }
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(exited(exit, &log_path));
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(UP_POLL_MILLIS));
    }
    let why = format!(
        "{} did not answer in {UP_SECONDS_MAX} s",
        socket.path().display()
    );
    Err(CliError::NotStarted(why))
}

fn not_started(
    child: &mut std::process::Child,
    log_path: &Path,
    deadline: Instant,
    error: ClientError,
) -> CliError {
    assert!(log_path.is_absolute());
    let polls = UP_SECONDS_MAX * 1000 / UP_POLL_MILLIS;
    for _ in 0..polls {
        match child.try_wait() {
            Ok(Some(exit)) => return exited(exit, log_path),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(UP_POLL_MILLIS));
            }
            Ok(None) | Err(_) => break,
        }
    }
    assert!(polls > 0);
    CliError::Client(error)
}

fn exited(exit: std::process::ExitStatus, log_path: &Path) -> CliError {
    assert!(log_path.is_absolute());
    let why = format!("it exited with {exit}: {}", last_line(log_path));
    CliError::NotStarted(format!("{why}; see {}", log_path.display()))
}

fn last_line(log_path: &Path) -> String {
    let mut tail = Vec::with_capacity(UP_LOG_TAIL_BYTES);
    let read = File::open(log_path).and_then(|mut file| {
        let len = file.metadata()?.len();
        let start = len.saturating_sub(UP_LOG_TAIL_BYTES as u64);
        io::Seek::seek(&mut file, io::SeekFrom::Start(start))?;
        file.take(UP_LOG_TAIL_BYTES as u64).read_to_end(&mut tail)
    });
    let text = String::from_utf8_lossy(&tail);
    let line = text.lines().rev().find(|line| !line.trim().is_empty());
    let line = match (read, line) {
        (Ok(_), Some(line)) => line.trim().to_string(),
        (Ok(_), None) => "its log is empty".to_string(),
        (Err(error), _) => format!("its log could not be read: {error}"),
    };
    assert!(!line.is_empty() && line.len() <= 4 * UP_LOG_TAIL_BYTES);
    line
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
    println!("spent {}", usage_text(&data["usage"]));
    let list = |key: &str| data[key].as_array().map_or(&[][..], Vec::as_slice);
    let (tasks, spent, used) = (list("tasks"), list("projects"), list("task_usages"));
    let name = |value: &serde_json::Value| -> String {
        value["project"]
            .as_str()
            .unwrap_or(daemon::SUBTREE)
            .to_string()
    };
    let mut projects: Vec<String> = tasks.iter().chain(spent).chain(used).map(name).collect();
    projects.sort_unstable();
    projects.dedup();
    for project in projects {
        match spent.iter().find(|entry| name(entry) == project) {
            Some(entry) => println!("{project} spent {}", usage_text(&entry["usage"])),
            None => println!("{project}"),
        }
        let usage_of = |id: &serde_json::Value| {
            let entry = used.iter().find(|entry| entry["id"] == *id);
            entry.map_or(String::new(), |entry| {
                format!(" spent {}", usage_text(&entry["usage"]))
            })
        };
        for task in tasks.iter().filter(|task| name(task) == project) {
            println!("{}{}", task_row(task), usage_of(&task["id"]));
        }
        let settled = (used.iter()).filter(|entry| {
            name(entry) == project && tasks.iter().all(|task| task["id"] != entry["id"])
        });
        for entry in settled {
            println!("  #{}{}", entry["id"], usage_of(&entry["id"]));
        }
    }
    Ok(0)
}

// Tokens and cost as `status` shows them; a cost missing for some model is said to be.
fn usage_text(usage: &serde_json::Value) -> String {
    let count = |kind: &str| usage[kind].as_u64().unwrap_or(0);
    let micros = count("cost_micros");
    let cost = format!("${}.{:02}", micros / 1_000_000, micros % 1_000_000 / 10_000);
    let cost = match (usage["unpriced"] == true, micros) {
        (false, _) => cost,
        (true, 0) => "cost unknown".to_string(),
        (true, _) => format!("{cost} and some unpriced"),
    };
    format!(
        "{} in, {} cache read, {} cache write, {} out, {cost}",
        count("input"),
        count("cache_read"),
        count("cache_write"),
        count("output"),
    )
}

fn task_row(task: &serde_json::Value) -> String {
    format!(
        "  #{} {} {} {} {} {}",
        task["id"],
        text(&task["state"]),
        text(&task["role"]),
        text(&task["profile"]),
        age(&task["age_ms"]),
        text(&task["branch"]),
    )
}

fn queue(config: &Config, json: bool) -> Result<u8, CliError> {
    let repos = queue::opted_in(&config.home).map_err(CliError::Queue)?;
    if repos.is_empty() {
        let path = config.home.join(STAND_IN_FILE);
        say(&format!(
            "no repository is opted in to the queue; add to {}:",
            path.display()
        ));
        say("[repo.\"/path/to/checkout\"]\nqueue = true");
        return Ok(1);
    }
    let board = VbCli {
        vb: config.vb_dir().join("vb"),
        url: config.board_url.clone(),
        profile: QUEUE_PROFILE.to_string(),
    };
    let listings = thread::scope(|scope| {
        let reads: Vec<_> = [QUEUE_CHANNEL, DECISIONS]
            .map(|channel| scope.spawn(|| open_tasks(&board, channel)))
            .into();
        let joined = reads.into_iter().map(|read| {
            let read = read.join();
            read.unwrap_or_else(|_| Err(BoardError::Io("a board read panicked".into())))
        });
        joined.collect::<Result<Vec<_>, BoardError>>()
    });
    let mut tasks = Vec::new();
    for listing in listings.map_err(CliError::Board)? {
        let total = listing.tasks.len().saturating_add(listing.left_out);
        if total.saturating_add(tasks.len()) > QUEUE_ITEMS_MAX {
            return Err(CliError::Queue(QueueError::TooManyItems));
        }
        tasks.extend(listing.tasks);
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let now = u64::try_from(now).unwrap_or(0);
    let view = queue::view(&tasks, now).map_err(CliError::Queue)?;
    if json {
        say(&view.json.to_string());
        return Ok(0);
    }
    let mut lines = view.lines;
    lines.push(String::new());
    lines.extend(running(config));
    let mut out = io::stdout().lock();
    let written = lines.iter().try_for_each(|line| writeln!(out, "{line}"));
    written
        .and_then(|()| out.flush())
        .map_err(|error| io_error("write", Path::new("standard output"), &error))?;
    Ok(0)
}

// The open tasks in `channel`; none when the channel does not exist yet.
fn open_tasks(board: &VbCli, channel: &str) -> Result<Listing, BoardError> {
    match board.list_labelled(channel, "board.task", &OPEN_STATES) {
        Err(BoardError::Exit { stderr, .. })
            if board::parse_failure_reason(&stderr).as_deref() == Some(CHANNEL_NOT_FOUND) =>
        {
            Ok(Listing {
                tasks: Vec::new(),
                cursor: Cursor(0),
                left_out: 0,
            })
        }
        listed => listed,
    }
}

fn running(config: &Config) -> Vec<String> {
    let Ok(mut client) = Client::connect(&config.home) else {
        return vec!["daemon not running".to_string()];
    };
    let Ok(data) = client.send("status", &[]) else {
        return vec!["running work unavailable: the daemon did not answer".to_string()];
    };
    let mut lines = vec![format!("running {} of {}", data["running"], data["limit"])];
    let tasks = data["tasks"].as_array().map_or(&[][..], Vec::as_slice);
    let claimed = tasks.iter().filter(|task| task["state"] == "claimed");
    lines.extend(claimed.map(task_row));
    assert!(!lines.is_empty());
    lines
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
            None => profile == LEAD_PROFILE || profile.starts_with(LEAD_PREFIX),
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

struct Finished {
    repo: PathBuf,
    branch: String,
    head: String,
    title: String,
    summary: String,
    base: Option<String>,
}

struct GitHub {
    host: String,
    owner: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePr {
    url: String,
    #[serde(rename = "headRefName")]
    head_ref: String,
    #[serde(rename = "headRepository")]
    head_repository: Option<WireRepoName>,
    #[serde(rename = "headRepositoryOwner")]
    head_owner: Option<WireOwner>,
}

#[derive(Deserialize)]
struct WireRepoName {
    name: String,
}

#[derive(Deserialize)]
struct WireOwner {
    login: String,
}

fn label<'a>(view: &'a TaskView, key: &str) -> Option<&'a str> {
    let found = view.labels.iter().find(|(name, _)| name == key);
    found.map(|(_, value)| value.as_str())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettledRecord {
    repo: String,
    branch: Option<String>,
    head: String,
    summary: String,
    base: Option<String>,
}

fn settled(home: &Path, id: TaskId) -> Result<SettledRecord, CliError> {
    let refuse = |why| CliError::NotPushable { id, why };
    let path = result_path(home, id);
    let io = |error: io::Error| CliError::Io {
        action: "read",
        path: path.clone(),
        message: error.to_string(),
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(refuse(
                "has no result Rider recorded when it settled the task",
            ));
        }
        Err(error) => return Err(io(error)),
    };
    let mut bytes = Vec::new();
    file.take(RESULT_FILE_BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > RESULT_FILE_BYTES_MAX {
        return Err(refuse("has a recorded result larger than Rider writes"));
    }
    let record: SettledRecord = serde_json::from_slice(&bytes)
        .map_err(|_| refuse("has a recorded result Rider cannot read"))?;
    assert!(bytes.len() as u64 <= RESULT_FILE_BYTES_MAX);
    Ok(record)
}

fn finished(view: &TaskView, home: &Path) -> Result<Finished, CliError> {
    let id = view.id;
    let refuse = |why| CliError::NotPushable { id, why };
    if view.state != TaskState::Completed {
        return Err(refuse("is not completed"));
    }
    let record = settled(home, id)?;
    let repo = PathBuf::from(record.repo);
    if !repo.is_absolute() {
        return Err(refuse(
            "has a recorded repository that is not an absolute path",
        ));
    }
    let branch = record.branch.filter(|branch| git::is_plain_branch(branch));
    let branch = branch.ok_or_else(|| refuse("has no result branch naming one branch"))?;
    if label(view, "branch") != Some(branch.as_str()) {
        return Err(refuse(
            "has a branch label that differs from its result's branch",
        ));
    }
    if !git::is_commit_id(&record.head) {
        return Err(refuse("has a result head that is not a commit id"));
    }
    let first = view.text.lines().next().unwrap_or_default().trim();
    let title: String = first.chars().take(PR_TITLE_CHARS_MAX).collect();
    if title.is_empty() || record.summary.trim().is_empty() {
        return Err(refuse("has no first line or no result summary for its PR"));
    }
    assert!(repo.is_absolute() && git::is_plain_branch(&branch));
    assert!(title.chars().count() <= PR_TITLE_CHARS_MAX);
    Ok(Finished {
        repo,
        branch,
        head: record.head,
        title,
        summary: record.summary,
        base: record.base,
    })
}

struct Chain {
    root: TaskId,
    base: String,
}

fn chain(
    board: &dyn Board,
    view: &TaskView,
    home: &Path,
    task: &Finished,
) -> Result<Chain, CliError> {
    let refuse = |why| CliError::NotPushable { id: view.id, why };
    let mut root = view.id;
    let mut steps = 0_usize;
    let mut current = view.clone();
    while let Some(onto) = label(&current, "onto") {
        let onto = parse_task_id(onto).map_err(|_| refuse("has a bad onto"))?;
        steps += 1;
        if steps > BASE_CHAIN_MAX {
            return Err(refuse("has an onto chain longer than rider pr follows"));
        }
        current = board.read_task(onto).map_err(CliError::Board)?;
        root = current.id;
    }
    let base = match &task.base {
        Some(base) => base.clone(),
        None if root != view.id => {
            let settled = settled(home, root)?;
            if Path::new(&settled.repo) != task.repo {
                return Err(refuse("has an onto chain in another repository"));
            }
            settled
                .base
                .ok_or_else(|| refuse("has an onto chain without a recorded base"))?
        }
        None => return Err(refuse("has no base branch Rider recorded")),
    };
    if !git::is_plain_branch(&base) {
        return Err(refuse("has a recorded base that is not one plain branch"));
    }
    assert!(steps <= BASE_CHAIN_MAX);
    assert!(git::is_plain_branch(&base));
    Ok(Chain { root, base })
}

fn github(repo: &Path, id: TaskId) -> Result<GitHub, CliError> {
    let refuse = |why| CliError::NotPushable { id, why };
    let url = git::origin_url(repo).map_err(CliError::Git)?;
    let url = url.ok_or_else(|| {
        refuse("has an origin whose configured, fetch and push URLs are not one and the same")
    })?;
    let rest = ["https://", "ssh://git@"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme))
        .and_then(|rest| rest.split_once('/'))
        .or_else(|| {
            url.strip_prefix("git@")
                .and_then(|rest| rest.split_once(':'))
        });
    let (host, path) = rest.ok_or_else(|| refuse("has an origin that is not a GitHub URL"))?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let word = |text: &str| {
        !text.is_empty()
            && !text.starts_with(['-', '.'])
            && (text.bytes()).all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    let parsed = path
        .split_once('/')
        .filter(|(owner, name)| word(owner) && word(name));
    let Some((owner, name)) = parsed.filter(|_| word(host) && !host.contains('@')) else {
        return Err(refuse("has an origin that is not a GitHub URL"));
    };
    assert!(word(owner) && word(name));
    Ok(GitHub {
        host: host.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn publish(task: &Finished, id: TaskId) -> Result<(), CliError> {
    let refuse = |why| CliError::NotPushable { id, why };
    let (repo, branch, head) = (&task.repo, &task.branch, &task.head);
    if !git::commit_exists(repo, head).map_err(CliError::Git)? {
        return Err(refuse("has a result head this repository does not hold"));
    }
    let local = git::commit_of(repo, &format!("refs/heads/{branch}")).map_err(CliError::Git)?;
    if !git::is_ancestor(repo, head, &local).map_err(CliError::Git)? {
        return Err(refuse("has a result head that is not on its local branch"));
    }
    match git::remote_branch(repo, branch).map_err(CliError::Git)? {
        Some(tip) if tip == *head => return Ok(()),
        Some(tip) => {
            let seen = git::tracking_branch(repo, branch).map_err(CliError::Git)?;
            if seen.as_deref() != Some(tip.as_str()) {
                return Err(refuse(
                    "has a branch that moved on origin since this repository saw it",
                ));
            }
            if !git::is_ancestor(repo, &tip, head).map_err(CliError::Git)? {
                return Err(refuse(
                    "has a branch on origin that its result does not contain",
                ));
            }
        }
        None => {}
    }
    assert!(git::is_commit_id(head));
    git::push_commit(repo, head, branch).map_err(CliError::Git)
}

fn pr(config: &Config, id: TaskId) -> Result<u8, CliError> {
    let board = VbCli {
        vb: config.vb_dir().join("vb"),
        url: config.board_url.clone(),
        profile: PR_PROFILE.to_string(),
    };
    let view = board.read_task(id).map_err(CliError::Board)?;
    let task = finished(&view, config.home())?;
    let Chain { root, base } = chain(&board, &view, config.home(), &task)?;
    let own = format!("rider/{}-", root.0);
    let default = git::default_branch(&task.repo).map_err(CliError::Git)?;
    if !task.branch.starts_with(&own)
        || base == task.branch
        || default.as_ref() == Some(&task.branch)
    {
        let why = "has a branch that is not the one Rider made for it";
        return Err(CliError::NotPushable { id, why });
    }
    let hub = github(&task.repo, id)?;
    publish(&task, id)?;
    let repo_arg = format!("--repo={}/{}/{}", hub.host, hub.owner, hub.name);
    let head_arg = format!("--head={}", task.branch);
    let limit = format!("--limit={PR_LIST_MAX}");
    let list = [
        "pr",
        "list",
        &repo_arg,
        &head_arg,
        "--state=open",
        PR_FIELDS,
        &limit,
    ];
    let listed = git::gh(&task.repo, &list.map(OsStr::new), None).map_err(CliError::Git)?;
    let bad = |bytes: &[u8]| CliError::BadGh(String::from_utf8_lossy(bytes).into_owned());
    let open: Vec<WirePr> = serde_json::from_slice(&listed).map_err(|_| bad(&listed))?;
    if open.len() >= PR_LIST_MAX {
        return Err(bad(&listed));
    }
    let ours = |pr: &WirePr| {
        pr.head_ref == task.branch
            && (pr.head_owner.as_ref()).is_some_and(|o| o.login.eq_ignore_ascii_case(&hub.owner))
            && (pr.head_repository.as_ref()).is_some_and(|r| r.name.eq_ignore_ascii_case(&hub.name))
    };
    let mut matching = open.into_iter().filter(ours);
    let found = matching.next();
    if matching.next().is_some() {
        return Err(bad(&listed));
    }
    let url = match found {
        Some(found) => found.url,
        None => {
            let title = format!("--title={}", task.title);
            let head = format!("--head={}:{}", hub.owner, task.branch);
            let base = format!("--base={base}");
            let create = [
                "pr",
                "create",
                &repo_arg,
                "--draft",
                &head,
                &title,
                &base,
                "--body-file",
                "-",
            ];
            let body = Some(task.summary.into_bytes());
            let args = create.map(OsStr::new);
            let created = git::gh(&task.repo, &args, body).map_err(CliError::Git)?;
            String::from_utf8_lossy(&created).trim().to_string()
        }
    };
    if !url.starts_with("https://") || url.contains(char::is_whitespace) {
        return Err(CliError::BadGh(url));
    }
    assert!(!url.is_empty());
    say(&url);
    Ok(0)
}
