//! Launches one agent, records its output to a size-capped log, stops it at a time limit, and
//! reads its final message back from the recorded stream.

use crate::board::{message_tail, one_line};
use crate::git;
use crate::roles::{AGENTS_MAX, Harness, RoleSpec};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::process::Signal;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::collections::VecDeque;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, SystemTimeError};

/// An attempt is killed after this long.
pub const ATTEMPT_SECONDS_MAX: u64 = 55 * 60;
/// Upper bound on one attempt's log; output past it is discarded.
pub const LOG_BYTES_MAX: u64 = 64 * 1024 * 1024;
/// Upper bound on a prompt.
pub const PROMPT_BYTES_MAX: usize = 256 * 1024;
/// How long an agent has to exit after SIGTERM before its group gets SIGKILL.
pub const KILL_GRACE_SECONDS: u64 = 10;
/// Upper bound on the MCP servers one Codex configuration file declares.
pub const MCP_SERVERS_MAX: usize = 32;
const MCP_NAME_BYTES_MAX: usize = 64;
const CODEX_CONFIG_BYTES_MAX: u64 = 1024 * 1024;
// Each user server takes one setting and each project server two.
const MCP_SETTINGS_MAX: usize = 3 * MCP_SERVERS_MAX;
const ARGS_MAX: usize = 28 + 2 * MCP_SETTINGS_MAX;
const OPEN_TRIES_MAX: usize = 2;
const UNTRUSTED_ENV_ALLOWED: [&str; 10] = [
    "HOME", "PATH", "USER", "LOGNAME", "SHELL", "TMPDIR", "TZ", "LANG", "LC_ALL", "LC_CTYPE",
];
const CODEX_REFUSAL_BYTES_MAX: usize = 64 * 1024;
const CODEX_NO_ROLLOUT: &str = "no rollout found for thread id";
const ENV_MAX: usize = 16;
const POLL_MILLIS: u64 = 50;
// Every poll sleeps `POLL_MILLIS`, so the final kill comes before the last poll.
const POLLS_MAX: u64 = (ATTEMPT_SECONDS_MAX + KILL_GRACE_SECONDS) * 1000 / POLL_MILLIS + 16;
const CHUNK_BYTES: usize = 64 * 1024;
const READS_MAX: u64 = 1 << 32;
const READERS: usize = 2;
const DRAIN_SECONDS_MAX: u64 = 5;
const STDIN_WRITERS: usize = 1;
// Pipe threads alive across all runs: two readers and a stdin writer per running agent, plus
// room for threads left blocked on a pipe that a process outside the agent's group holds open.
const READER_THREADS_MAX: usize = 4 * (READERS + STDIN_WRITERS) * AGENTS_MAX as usize;
const LINE_BYTES_MAX: usize = 4 * 1024 * 1024;
// Every line holds at least its newline, so a log read up to `LOG_BYTES_MAX` has fewer lines.
const LINES_MAX: u64 = LOG_BYTES_MAX + 1;
const SKIP_READS_MAX: u64 = LOG_BYTES_MAX / LINE_BYTES_MAX as u64 + 2;
const REVIEW_INTERRUPTED: &str = "Review was interrupted.";
// Codex's content-filter error has no code: its message's first sentence names it.
const CONTENT_FILTER: &str = "This content was flagged for possible cybersecurity risk.";
const STDERR_TAIL_BYTES: u64 = 64 * 1024;
const STDERR_FINAL_LINES: usize = 8;
pub const HANDOFF_LINES_MAX: usize = 24;
const CLIP_CHARS: usize = 160;
pub const HANDOFF_LOG_LINE_CHARS_MAX: usize = 2 * CLIP_CHARS + 64;
const POISONED: &str = "the log lock was poisoned";

const _: () = {
    assert!(LINES_MAX > LOG_BYTES_MAX);
    assert!(SKIP_READS_MAX * LINE_BYTES_MAX as u64 > LOG_BYTES_MAX);
    assert!(DRAIN_SECONDS_MAX > 0 && POLL_MILLIS > 0);
    assert!(POLLS_MAX * POLL_MILLIS > (ATTEMPT_SECONDS_MAX + KILL_GRACE_SECONDS) * 1000);
    assert!(READER_THREADS_MAX > (READERS + STDIN_WRITERS) * AGENTS_MAX as usize);
};

static READER_THREADS: AtomicUsize = AtomicUsize::new(0);

/// A program and its arguments, ready to run in a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub program: String,
    pub args: Vec<String>,
    /// Set on top of the inherited environment.
    pub env: Vec<(String, String)>,
    pub untrusted: bool,
    pub io: Io,
}

/// What an agent reads and where its output goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Io {
    /// Standard input is closed; standard output and error both go to the log.
    Stream,
    /// The prompt is fed on standard input; standard output, the final text, goes to the log
    /// and standard error, the transcript, to `stderr_log(log)`.
    PromptOnStdin(String),
}

/// Where an `Io::PromptOnStdin` agent's standard error is logged: beside `log`.
pub fn stderr_log(log: &Path) -> PathBuf {
    let mut path = log.as_os_str().to_owned();
    path.push(".stderr");
    let path = PathBuf::from(path);
    assert!(path != log && path.parent() == log.parent());
    path
}

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// A signal rider did not send ended the agent.
    Signal,
    /// Rider stopped the agent at the time limit, however it then exited.
    TimedOut,
}

/// Why an agent could not be started, watched, or logged.
#[derive(Debug)]
pub enum HarnessError {
    /// The agent or one of its reader threads could not start.
    Spawn(String),
    /// The log could not be written, or the agent's output could not be read.
    Log(String),
    /// Waiting for the agent failed; rider killed it.
    Wait(String),
    PromptTooLarge {
        bytes: usize,
    },
    /// The prompt is blank or one word, which a CLI could read as a subcommand or as stdin.
    PromptUnsafe,
    /// Too many reader threads are still blocked on pipes from earlier runs.
    ReadersExhausted,
    /// The user's Codex configuration could not be read, or its MCP servers are not ones
    /// Rider can turn off.
    CodexConfig(String),
}

impl fmt::Display for HarnessError {
    /// One line saying why the agent could not be run, with system text cut short.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HarnessError::Spawn(message) => {
                write!(f, "the agent could not be started: {}", one_line(message))
            }
            HarnessError::Log(message) => {
                write!(f, "the agent's log failed: {}", one_line(message))
            }
            HarnessError::Wait(message) => {
                write!(f, "waiting for the agent failed: {}", one_line(message))
            }
            HarnessError::PromptTooLarge { bytes } => write!(
                f,
                "the prompt is {bytes} bytes, over the limit of {PROMPT_BYTES_MAX}"
            ),
            HarnessError::PromptUnsafe => {
                write!(
                    f,
                    "the prompt is blank or one word, which a CLI could misread"
                )
            }
            HarnessError::ReadersExhausted => write!(
                f,
                "too many reader threads are still blocked on pipes from earlier runs"
            ),
            HarnessError::CodexConfig(message) => {
                write!(f, "the Codex configuration {}", one_line(message))
            }
        }
    }
}

/// Upper bound on a session id's length.
pub const SESSION_BYTES_MAX: usize = 128;

/// A Claude session or Codex thread id: ASCII letters, digits, and `-`, never starting with `-`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    /// The id in `text`, or `None` when it is not one.
    pub fn parse(text: &str) -> Option<SessionId> {
        let valid = !text.is_empty()
            && text.len() <= SESSION_BYTES_MAX
            && !text.starts_with('-')
            && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let id = valid.then(|| SessionId(text.to_string()));
        assert!(id.as_ref().is_none_or(|id| id.0 == text));
        id
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Resume<'a> {
    Fork(&'a SessionId),
    InPlace(&'a SessionId),
}

/// The `-c` settings that turn off every MCP server the user's Codex configuration and the
/// worktree's project configuration declare; none for Claude, which takes an empty strict MCP
/// configuration instead.
pub fn codex_mcp_overrides(
    harness: &Harness,
    dir: &Path,
    untrusted: bool,
) -> Result<Vec<String>, HarnessError> {
    assert!(dir.is_absolute(), "agents run in absolute worktrees");
    assert!(!UNTRUSTED_ENV_ALLOWED.contains(&"CODEX_HOME"));
    if *harness == Harness::Claude {
        return Ok(Vec::new());
    }
    // Codex takes an empty `CODEX_HOME` as unset.
    let home = env::var_os("CODEX_HOME").filter(|home| !home.is_empty() && !untrusted);
    let home = match home.map(PathBuf::from) {
        Some(home) if !home.is_absolute() => {
            let why = "CODEX_HOME is not an absolute path".to_string();
            return Err(HarnessError::CodexConfig(why));
        }
        Some(home) => Some(home),
        None => env::var_os("HOME").map(|home| Path::new(&home).join(".codex")),
    };
    let mut settings = Vec::new();
    if let Some(home) = home {
        for (name, _) in mcp_servers(&home.join("config.toml"))? {
            settings.push(format!("mcp_servers.{name}.enabled=false"));
        }
    }
    // A trusted repository's linked worktree loads the branch's `.codex/config.toml`, and naming
    // a server no loaded layer declares is an error, so each project server is turned off and
    // given a stand-in transport of its own kind, which holds whether the project is trusted.
    for (name, url) in mcp_servers(&dir.join(".codex").join("config.toml"))? {
        settings.push(format!("mcp_servers.{name}.enabled=false"));
        settings.push(if url {
            format!(r#"mcp_servers.{name}.url="http://127.0.0.1:1/""#)
        } else {
            format!(r#"mcp_servers.{name}.command="true""#)
        });
    }
    assert!(settings.len() <= MCP_SETTINGS_MAX);
    assert!(
        settings
            .iter()
            .all(|setting| setting.starts_with("mcp_servers."))
    );
    Ok(settings)
}

/// The MCP servers the Codex configuration at `path` declares, each with whether it is reached
/// by URL; none when there is no file. An error names the file and at most a line number,
/// never its content, which may hold tokens.
fn mcp_servers(path: &Path) -> Result<Vec<(String, bool)>, HarnessError> {
    let error = |why: &str| HarnessError::CodexConfig(format!("{} {why}", path.display()));
    let file = match File::open(path) {
        Ok(file) => file,
        Err(missing) if missing.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(error("cannot be read")),
    };
    let mut text = String::new();
    let read = file
        .take(CODEX_CONFIG_BYTES_MAX + 1)
        .read_to_string(&mut text);
    read.map_err(|_| error("cannot be read as UTF-8 text"))?;
    if u64::try_from(text.len()).map_or(true, |bytes| bytes > CODEX_CONFIG_BYTES_MAX) {
        return Err(error(&format!("is over {CODEX_CONFIG_BYTES_MAX} bytes")));
    }
    let table: toml::Table = toml::from_str(&text).map_err(|bad| {
        let at = bad.span().map_or(0, |span| span.start).min(text.len());
        let line = text.as_bytes()[..at]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        error(&format!("is not valid TOML at line {line}"))
    })?;
    let servers = match table.get("mcp_servers") {
        None => return Ok(Vec::new()),
        Some(toml::Value::Table(servers)) => servers,
        Some(_) => return Err(error("holds an `mcp_servers` that is not a table")),
    };
    if servers.len() > MCP_SERVERS_MAX {
        return Err(error(&format!(
            "declares over {MCP_SERVERS_MAX} MCP servers"
        )));
    }
    let valid = |name: &str| {
        !name.is_empty()
            && name.len() <= MCP_NAME_BYTES_MAX
            && (name.bytes()).all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    let mut found = Vec::with_capacity(servers.len());
    for (name, server) in servers {
        let toml::Value::Table(server) = server else {
            return Err(error("declares an MCP server that is not a table"));
        };
        if !valid(name) {
            let why = "names an MCP server with characters other than letters, digits, _ and -";
            return Err(error(why));
        }
        found.push((name.clone(), server.contains_key("url")));
    }
    assert!(found.len() <= MCP_SERVERS_MAX);
    assert!(found.iter().all(|(name, _)| valid(name)));
    Ok(found)
}

pub fn command_for(
    spec: &RoleSpec,
    prompt: &str,
    dir: &Path,
    resume: Option<Resume>,
    mcp_overrides: &[String],
) -> Result<CommandLine, HarnessError> {
    assert!(!spec.model.is_empty() && !spec.model.starts_with('-'));
    assert!(dir.is_absolute(), "agents run in absolute worktrees");
    assert!(mcp_overrides.len() <= MCP_SETTINGS_MAX);
    assert!(
        mcp_overrides.is_empty() || spec.harness != Harness::Claude,
        "Claude takes an empty strict MCP configuration instead"
    );
    assert!(dir.to_str().is_some(), "worktree paths are UTF-8");
    if prompt.len() > PROMPT_BYTES_MAX {
        let bytes = prompt.len();
        return Err(HarnessError::PromptTooLarge { bytes });
    }
    // Even after `--`, claude may take a lone word such as `mcp` as a subcommand, and codex
    // reads stdin for `-`. A real prompt is a sentence, so it always holds whitespace.
    if prompt.trim().is_empty() || !prompt.contains(char::is_whitespace) {
        return Err(HarnessError::PromptUnsafe);
    }
    let dir = dir.to_string_lossy();
    let model = spec.model;
    let model_setting = format!("model={model:?}");
    let review_model_setting = format!("review_model={model:?}");
    let (program, mut args, io) = match spec.harness {
        Harness::Claude => (
            "claude",
            vec![
                "-p",
                "--model",
                model,
                "--output-format",
                "stream-json",
                "--verbose",
                "--dangerously-skip-permissions",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
            ],
            Io::Stream,
        ),
        Harness::Codex => (
            "codex",
            vec![
                "exec",
                "--model",
                model,
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--disable",
                "plugins",
                "--disable",
                "apps",
            ],
            Io::Stream,
        ),
        // `codex review` refuses a prompt with `--base`, `--commit` or `--uncommitted`, has no
        // bypass flag, and reads its prompt from stdin for `-`.
        Harness::CodexReview => (
            "codex",
            vec![
                "review",
                "-c",
                &model_setting,
                // The review subagent runs `review_model` when one is configured, not `model`.
                "-c",
                &review_model_setting,
                "-c",
                r#"sandbox_mode="danger-full-access""#,
                "-c",
                r#"approval_policy="never""#,
                "--disable",
                "plugins",
                "--disable",
                "apps",
                "-",
            ],
            Io::PromptOnStdin(prompt.to_string()),
        ),
    };
    // `-c mcp_servers={}` merges into the user's table instead of replacing it, so each
    // declared server is turned off by name.
    let off = (mcp_overrides.iter()).flat_map(|setting| ["-c", setting.as_str()]);
    args.splice(1..1, off);
    match (spec.harness, resume) {
        // Without `--fork-session` Claude appends to the resumed session in place.
        (Harness::Claude, Some(Resume::Fork(session))) => {
            args.extend(["--resume", &session.0, "--fork-session", "--", prompt]);
        }
        (Harness::Claude, Some(Resume::InPlace(session))) => {
            args.extend(["--resume", &session.0, "--", prompt]);
        }
        (Harness::Claude, None) => args.extend(["--", prompt]),
        // `codex exec resume` has no `-C` and no fork: it runs in the child's working
        // directory and continues the session in place.
        (Harness::Codex, Some(Resume::Fork(session) | Resume::InPlace(session))) => {
            args.insert(1, "resume");
            args.extend(["--", &session.0, prompt]);
        }
        (Harness::Codex, None) => args.extend(["-C", &dir, "--", prompt]),
        (Harness::CodexReview, _) => {}
    }
    let args: Vec<String> = args.into_iter().map(String::from).collect();
    assert!(args.len() <= ARGS_MAX);
    assert!(args.iter().any(|a| a == model || *a == model_setting));
    let program = program.to_string();
    let env = Vec::new();
    Ok(CommandLine {
        program,
        args,
        env,
        untrusted: false,
        io,
    })
}

struct LogFile {
    file: File,
    written: u64,
    cap: u64,
}

struct Shared {
    file: Mutex<LogFile>,
    // Checked under the file lock before each write, so no reader writes once it is set.
    closed: AtomicBool,
    // Never held across I/O, so the supervisor cannot be stalled by a hung write.
    error: Mutex<Option<String>>,
}

impl Shared {
    fn accept(&self, bytes: &[u8]) -> Result<(), String> {
        assert!(!bytes.is_empty() && bytes.len() <= CHUNK_BYTES);
        let mut log = self.file.lock().map_err(|_| POISONED.to_string())?;
        assert!(log.written <= log.cap);
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let room = log.cap - log.written;
        let count = usize::try_from(room).map_or(bytes.len(), |room| room.min(bytes.len()));
        if count == 0 {
            return Ok(());
        }
        let written = log.file.write_all(&bytes[..count]);
        written.map_err(|error| format!("writing the log: {error}"))?;
        let total = u64::try_from(count)
            .ok()
            .and_then(|c| log.written.checked_add(c));
        log.written = total
            .filter(|total| *total <= log.cap)
            .ok_or_else(|| "the log outgrew its cap".to_string())?;
        assert!(log.written <= log.cap);
        Ok(())
    }

    fn fail(&self, message: String) {
        assert!(!message.is_empty());
        if let Ok(mut slot) = self.error.lock() {
            slot.get_or_insert(message);
        }
    }

    fn error(&self) -> Option<String> {
        let found = match self.error.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => Some(POISONED.to_string()),
        };
        assert!(found.as_ref().is_none_or(|message| !message.is_empty()));
        found
    }

    fn close(&self) -> Option<String> {
        let was_closed = self.closed.swap(true, Ordering::SeqCst);
        assert!(!was_closed, "a log is closed once");
        self.error()
    }
}

// A FIFO would block a plain open and a symlink would redirect it, so the open neither waits
// nor follows, and the descriptor is checked. A log readers may already hold open from before
// it was private is replaced by a new file, not narrowed, so they see nothing written after.
pub enum PrivateOpenError {
    NotAFile,
    Replaced,
    Io(io::Error),
}

impl fmt::Display for PrivateOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrivateOpenError::NotAFile => write!(f, "it is not a regular file of the current user"),
            PrivateOpenError::Replaced => write!(f, "it was replaced while it was opened"),
            PrivateOpenError::Io(error) => write!(f, "{error}"),
        }
    }
}

pub fn open_private(path: &Path, options: &mut OpenOptions) -> Result<File, PrivateOpenError> {
    assert!(path.is_absolute());
    let flags = (OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits();
    options.create(true).mode(0o600).custom_flags(flags);
    let uid = rustix::process::getuid().as_raw();
    for _ in 0..OPEN_TRIES_MAX {
        // A nonblocking open for writing fails with ENXIO on a FIFO no process reads, and
        // O_NOFOLLOW fails with ELOOP on a symlink.
        let file = options
            .open(path)
            .map_err(|error| match error.raw_os_error() {
                Some(code) if code == Errno::ENXIO as i32 || code == Errno::ELOOP as i32 => {
                    PrivateOpenError::NotAFile
                }
                _ => PrivateOpenError::Io(error),
            })?;
        let metadata = file.metadata().map_err(PrivateOpenError::Io)?;
        if !metadata.is_file() || metadata.uid() != uid {
            return Err(PrivateOpenError::NotAFile);
        }
        if metadata.mode() & 0o077 == 0 && metadata.nlink() == 1 {
            let private = fs::Permissions::from_mode(0o600);
            file.set_permissions(private)
                .map_err(PrivateOpenError::Io)?;
            return Ok(file);
        }
        drop(file);
        fs::remove_file(path).map_err(PrivateOpenError::Io)?;
    }
    Err(PrivateOpenError::Replaced)
}

fn open_log(log: &Path, log_bytes_max: u64) -> Result<Arc<Shared>, HarnessError> {
    assert!(!log.as_os_str().is_empty());
    assert!(log_bytes_max <= LOG_BYTES_MAX);
    let failed = |error: io::Error| HarnessError::Log(format!("{}: {error}", log.display()));
    if let Some(parent) = log.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(failed)?;
    }
    let file = open_private(log, OpenOptions::new().write(true));
    let file = file.map_err(|error| HarnessError::Log(format!("{}: {error}", log.display())))?;
    file.set_len(0).map_err(failed)?;
    let cap = log_bytes_max;
    Ok(Arc::new(Shared {
        file: Mutex::new(LogFile {
            file,
            written: 0,
            cap,
        }),
        closed: AtomicBool::new(false),
        error: Mutex::new(None),
    }))
}

struct ReaderSlot {
    live: &'static AtomicUsize,
}

impl ReaderSlot {
    fn reserve(live: &'static AtomicUsize, max: usize) -> Option<ReaderSlot> {
        assert!(max > 0);
        let before = live.fetch_add(1, Ordering::SeqCst);
        if before >= max {
            live.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        assert!(before < max);
        Some(ReaderSlot { live })
    }
}

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        let before = self.live.fetch_sub(1, Ordering::SeqCst);
        assert!(before > 0, "every slot was counted when reserved");
        assert!(
            before <= READER_THREADS_MAX,
            "no reserve outgrows the largest bound"
        );
    }
}

fn pump(stream: &mut impl Read, shared: &Shared) -> Result<(), String> {
    let mut buffer = vec![0u8; CHUNK_BYTES];
    for _ in 0..READS_MAX {
        let count = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("reading the agent's output: {error}")),
        };
        assert!(count <= CHUNK_BYTES);
        shared.accept(&buffer[..count])?;
    }
    Err(format!("the agent's output took over {READS_MAX} reads"))
}

fn start_reader(
    mut stream: impl Read + Send + 'static,
    name: &str,
    slot: ReaderSlot,
    shared: &Arc<Shared>,
    done: &SyncSender<Result<(), String>>,
) -> Result<(), HarnessError> {
    assert!(name == "stdout" || name == "stderr");
    assert!(slot.live.load(Ordering::SeqCst) > 0);
    let (shared, done) = (Arc::clone(shared), done.clone());
    let builder = thread::Builder::new().name(format!("agent-{name}"));
    let started = builder.spawn(move || {
        let _slot = slot;
        let result = pump(&mut stream, &shared);
        if let Err(message) = &result {
            shared.fail(message.clone());
        }
        // The channel has room for every reader; it is gone only once rider stopped waiting.
        let _ = done.try_send(result);
    });
    started.map_err(|error| HarnessError::Spawn(format!("starting the {name} reader: {error}")))?;
    Ok(())
}

fn kill_and_reap(child: &mut dyn ChildWrapper) -> Result<ExitStatus, HarnessError> {
    assert!(child.id() > 1, "the group of a child rider started");
    // An empty group has nothing left to stop. The leader is killed on its own when its
    // group refuses the signal; that fails only when the agent already exited.
    if child.start_kill().is_err() {
        let _ = child.inner_mut().start_kill();
    }
    let status = child.wait();
    status.map_err(|error| HarnessError::Wait(format!("{error}")))
}

fn timed_out(
    monotonic: Duration,
    wall: Result<Duration, SystemTimeError>,
    limit: Duration,
) -> bool {
    assert!(limit > Duration::ZERO);
    // The board's lease runs on the wall clock, which goes on while the machine sleeps and
    // `Instant` does not. A wall clock set backwards counts as no time passed.
    let wall = wall.unwrap_or(Duration::ZERO);
    monotonic >= limit || wall >= limit
}

fn supervise(
    child: &mut dyn ChildWrapper,
    logs: [&Arc<Shared>; READERS],
    seconds_max: u64,
    wall_clock: &dyn Fn() -> SystemTime,
) -> Result<(ExitStatus, bool), HarnessError> {
    assert!(seconds_max > 0 && seconds_max <= ATTEMPT_SECONDS_MAX);
    assert!(child.id() > 1, "the group of a child rider started");
    let start = (Instant::now(), wall_clock());
    let limit = Duration::from_secs(seconds_max);
    let grace = Duration::from_secs(KILL_GRACE_SECONDS);
    let mut term_sent: Option<(Instant, SystemTime)> = None;
    for _ in 0..POLLS_MAX {
        let polled = match child.try_wait() {
            Ok(polled) => polled,
            Err(error) => {
                let _ = kill_and_reap(child);
                return Err(HarnessError::Wait(format!("{error}")));
            }
        };
        if let Some(status) = polled {
            return Ok((status, term_sent.is_some()));
        }
        if let Some(message) = logs.iter().find_map(|log| log.error()) {
            let _ = kill_and_reap(child);
            return Err(HarnessError::Log(message));
        }
        let now = (Instant::now(), wall_clock());
        let since = |then: (Instant, SystemTime)| {
            let monotonic = now.0.saturating_duration_since(then.0);
            (monotonic, now.1.duration_since(then.1))
        };
        match term_sent {
            None => {
                let (monotonic, wall) = since(start);
                if timed_out(monotonic, wall, limit) {
                    // An empty group has nothing left to stop.
                    let _ = child.signal(Signal::TERM.as_raw());
                    term_sent = Some(now);
                }
            }
            Some(sent) => {
                let (monotonic, wall) = since(sent);
                if timed_out(monotonic, wall, grace) {
                    return Ok((kill_and_reap(child)?, true));
                }
            }
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
    let _ = kill_and_reap(child);
    Err(HarnessError::Wait(format!(
        "the agent outlived {POLLS_MAX} polls"
    )))
}

fn drain(done: &Receiver<Result<(), String>>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(DRAIN_SECONDS_MAX);
    for _ in 0..READERS {
        let left = deadline.saturating_duration_since(Instant::now());
        match done.recv_timeout(left) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(message),
            // A process outside the agent's group holds a pipe; its reader is left behind.
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("a reader thread ended without reporting".to_string());
            }
        }
    }
    Ok(())
}

fn spawn(cmd: &CommandLine, dir: &Path) -> Result<Box<dyn ChildWrapper>, HarnessError> {
    assert!(!cmd.program.is_empty());
    assert!(cmd.args.len() <= ARGS_MAX && cmd.env.len() <= ENV_MAX);
    assert!(dir.is_absolute(), "agents run in absolute worktrees");
    let mut process = Command::new(&cmd.program);
    if cmd.untrusted {
        process.env_clear();
        for name in UNTRUSTED_ENV_ALLOWED {
            if let Some(value) = env::var_os(name) {
                process.env(name, value);
            }
        }
    }
    process
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(key, value)| (key, value)))
        .current_dir(dir)
        .stdin(match cmd.io {
            Io::Stream => Stdio::null(),
            Io::PromptOnStdin(_) => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in git::ENV_REMOVED {
        process.env_remove(name);
    }
    let spawned = CommandWrap::from(process)
        .wrap(ProcessGroup::leader())
        .spawn();
    spawned.map_err(|error| HarnessError::Spawn(format!("{}: {error}", cmd.program)))
}

fn start_readers(
    child: &mut dyn ChildWrapper,
    slots: [ReaderSlot; READERS],
    logs: [&Arc<Shared>; READERS],
    done: &SyncSender<Result<(), String>>,
) -> Result<(), HarnessError> {
    let missing = |name: &str| HarnessError::Spawn(format!("the agent has no {name}"));
    let stdout = child.stdout().take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr().take().ok_or_else(|| missing("stderr"))?;
    let [stdout_slot, stderr_slot] = slots;
    start_reader(stdout, "stdout", stdout_slot, logs[0], done)?;
    start_reader(stderr, "stderr", stderr_slot, logs[1], done)?;
    assert!(child.stdout().is_none() && child.stderr().is_none());
    Ok(())
}

fn feed(
    child: &mut dyn ChildWrapper,
    io: &Io,
    slot: Option<ReaderSlot>,
) -> Result<(), HarnessError> {
    let (Io::PromptOnStdin(prompt), Some(slot)) = (io, slot) else {
        assert!(child.stdin().is_none());
        return Ok(());
    };
    assert!(slot.live.load(Ordering::SeqCst) > 0);
    assert!(prompt.len() <= PROMPT_BYTES_MAX);
    let missing = || HarnessError::Spawn("the agent has no stdin".to_string());
    let mut stdin = child.stdin().take().ok_or_else(missing)?;
    let prompt = prompt.clone();
    // An agent that exits without reading closes the pipe, so the write ends; its exit status
    // says what happened.
    let builder = thread::Builder::new().name("agent-stdin".to_string());
    let started = builder.spawn(move || {
        let _slot = slot;
        let _ = stdin.write_all(prompt.as_bytes());
    });
    started.map_err(|error| HarnessError::Spawn(format!("starting the stdin writer: {error}")))?;
    assert!(child.stdin().is_none());
    Ok(())
}

/// Runs `cmd` in `dir`, logging at most `log_bytes_max` bytes, and stops it at `seconds_max`.
pub fn run(
    cmd: &CommandLine,
    dir: &Path,
    log: &Path,
    seconds_max: u64,
    log_bytes_max: u64,
) -> Result<Exit, HarnessError> {
    run_on_clock(cmd, dir, log, seconds_max, log_bytes_max, &SystemTime::now)
}

pub fn run_on_clock(
    cmd: &CommandLine,
    dir: &Path,
    log: &Path,
    seconds_max: u64,
    log_bytes_max: u64,
    wall_clock: &dyn Fn() -> SystemTime,
) -> Result<Exit, HarnessError> {
    assert!(seconds_max > 0 && seconds_max <= ATTEMPT_SECONDS_MAX);
    assert!(log_bytes_max <= LOG_BYTES_MAX);
    assert!(cmd.args.len() <= ARGS_MAX && cmd.env.len() <= ENV_MAX);
    let reserve = || {
        let slot = ReaderSlot::reserve(&READER_THREADS, READER_THREADS_MAX);
        slot.ok_or(HarnessError::ReadersExhausted)
    };
    let slots = [reserve()?, reserve()?];
    let writer = match cmd.io {
        Io::Stream => None,
        Io::PromptOnStdin(_) => Some(reserve()?),
    };
    let shared = open_log(log, log_bytes_max)?;
    let apart = match cmd.io {
        Io::Stream => None,
        Io::PromptOnStdin(_) => Some(open_log(&stderr_log(log), log_bytes_max)?),
    };
    let mut child = spawn(cmd, dir)?;
    assert!(
        child.id() > 1,
        "a spawned child is never init or the scheduler"
    );
    let (done, finished) = mpsc::sync_channel(READERS);
    let logs = [&shared, apart.as_ref().unwrap_or(&shared)];
    let started = start_readers(child.as_mut(), slots, logs, &done);
    if let Err(error) = started.and_then(|()| feed(child.as_mut(), &cmd.io, writer)) {
        let _ = kill_and_reap(child.as_mut());
        shared.close();
        apart.as_ref().map(|apart| apart.close());
        return Err(error);
    }
    drop(done);
    let outcome = supervise(child.as_mut(), logs, seconds_max, wall_clock);
    // The leader is reaped; leftover members of its group would hold the pipes open.
    let _ = child.start_kill();
    let drained = drain(&finished);
    let logged = shared.close();
    let logged = logged.or(apart.and_then(|apart| apart.close()));
    let (status, timed_out) = outcome?;
    if let Some(message) = logged {
        return Err(HarnessError::Log(message));
    }
    drained.map_err(HarnessError::Log)?;
    let exit = match status.code() {
        _ if timed_out => Exit::TimedOut,
        Some(code) => Exit::Code(code),
        None => Exit::Signal,
    };
    assert!(timed_out == (exit == Exit::TimedOut));
    Ok(exit)
}

fn skip_line(reader: &mut impl BufRead) -> bool {
    let mut scratch: Vec<u8> = Vec::new();
    let limit = u64::try_from(LINE_BYTES_MAX).unwrap_or(u64::MAX);
    for _ in 0..SKIP_READS_MAX {
        scratch.clear();
        match reader.by_ref().take(limit).read_until(b'\n', &mut scratch) {
            Ok(0) => return true,
            Ok(_) if scratch.last() == Some(&b'\n') => return true,
            Ok(count) => assert!(count == scratch.len() && count <= LINE_BYTES_MAX),
            Err(_) => return false,
        }
    }
    false
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Field<T> {
    Valid(T),
    Malformed(IgnoredAny),
}

fn valid<T>(field: Option<Field<T>>) -> Option<T> {
    match field? {
        Field::Valid(value) => Some(value),
        Field::Malformed(_) => None,
    }
}

// Agent streams gain fields and event types often: only what Rider reads is named.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudeEvent {
    #[serde(rename = "result")]
    Result {
        result: Option<Field<String>>,
        errors: Option<Field<Vec<String>>>,
        subtype: Option<Field<String>>,
        is_error: Option<Field<bool>>,
        num_turns: Option<Field<u64>>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        message: Option<Field<ClaudeMessage>>,
    },
    #[serde(rename = "user")]
    User {
        message: Option<Field<ClaudeMessage>>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClaudeUsageEvent {
    #[serde(rename = "result")]
    Result {
        usage: Option<Field<ClaudeUsage>>,
        total_cost_usd: Option<Field<f64>>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    content: Option<Field<Vec<Field<ClaudeBlock>>>>,
}

#[derive(Deserialize)]
struct ClaudeBlock {
    #[serde(rename = "type")]
    kind: Option<Field<String>>,
    text: Option<Field<String>>,
    is_error: Option<Field<bool>>,
    content: Option<Field<ToolContent>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ToolContent {
    Text(String),
    Blocks(Vec<Field<ClaudeText>>),
}

#[derive(Deserialize)]
struct ClaudeText {
    text: Option<Field<String>>,
}

#[derive(Deserialize)]
struct SessionLine {
    session_id: Option<Field<String>>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "item.started")]
    ItemStarted { item: Option<Field<CodexItem>> },
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: Option<Field<String>> },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: Option<Field<CodexItem>> },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: Option<Field<CodexError>> },
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: Option<Field<CodexUsage>> },
    #[serde(rename = "error")]
    Error { message: Option<Field<String>> },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
}

// Codex counts cached and cache-write tokens inside `input_tokens`.
#[derive(Deserialize)]
struct CodexUsage {
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    output_tokens: u64,
}

#[derive(Deserialize)]
struct CodexError {
    message: Option<Field<String>>,
}

#[derive(Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    kind: Option<Field<String>>,
    text: Option<Field<String>>,
    command: Option<Field<String>>,
    exit_code: Option<Field<i64>>,
    aggregated_output: Option<Field<String>>,
}

// Visits each line of `log` that is an `E`; skips other lines and those over `LINE_BYTES_MAX`.
// Returns whether the whole log was read.
fn scan<E: DeserializeOwned>(log: &Path, mut visit: impl FnMut(E)) -> bool {
    assert!(!log.as_os_str().is_empty());
    let Ok(file) = File::open(log) else {
        return false;
    };
    let mut reader = BufReader::new(file.take(LOG_BYTES_MAX));
    let mut line: Vec<u8> = Vec::new();
    let limit = u64::try_from(LINE_BYTES_MAX + 1).unwrap_or(u64::MAX);
    for _ in 0..LINES_MAX {
        line.clear();
        let Ok(count) = reader.by_ref().take(limit).read_until(b'\n', &mut line) else {
            return false;
        };
        if count == 0 {
            return true;
        }
        assert!(line.len() == count && count <= LINE_BYTES_MAX + 1);
        if count > LINE_BYTES_MAX && line.last() != Some(&b'\n') {
            if !skip_line(&mut reader) {
                return false;
            }
            continue;
        }
        if let Ok(event) = serde_json::from_slice::<E>(&line) {
            visit(event);
        }
    }
    false
}

/// The agent's trimmed final message, or `None` when the log is unreadable or has none.
pub fn last_message(harness: &Harness, log: &Path) -> Option<String> {
    let mut last: Option<String> = None;
    let mut set = |text: Option<String>| {
        assert!(text.as_ref().is_none_or(|t| t.len() <= LINE_BYTES_MAX));
        last = text;
    };
    let complete = match harness {
        Harness::Claude => scan(log, |event| {
            if let ClaudeEvent::Result { result, .. } = event {
                set(valid(result));
            }
        }),
        Harness::Codex => scan(log, |event| {
            let CodexEvent::ItemCompleted { item } = event else {
                return;
            };
            let Some(item) = valid(item) else {
                return;
            };
            if valid(item.kind).as_deref() == Some("agent_message") {
                set(valid(item.text));
            }
        }),
        // `codex review` prints no events: its standard output is the final text.
        Harness::CodexReview => {
            let mut text = Vec::new();
            let limit = u64::try_from(LINE_BYTES_MAX + 1).unwrap_or(u64::MAX);
            let read = File::open(log).and_then(|file| file.take(limit).read_to_end(&mut text));
            let fits = read.is_ok() && text.len() <= LINE_BYTES_MAX;
            if fits {
                set(String::from_utf8(text).ok());
            }
            fits
        }
    };
    if !complete {
        return None;
    }
    let text = last?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    assert!(text.len() <= LINE_BYTES_MAX);
    assert!(!text.starts_with(char::is_whitespace));
    Some(text)
}

/// The session id of the first event that carries one: a Claude event's `session_id`, or
/// Codex's `thread.started` `thread_id`. `None` when there is none.
pub fn session_id(harness: Harness, log: &Path) -> Option<SessionId> {
    let mut found: Option<Option<SessionId>> = None;
    let mut first = |id: Option<String>| {
        if found.is_none() {
            found = id.map(|id| SessionId::parse(&id));
        }
    };
    let _ = match harness {
        Harness::Claude => scan(log, |line: SessionLine| first(valid(line.session_id))),
        Harness::Codex => scan(log, |event| {
            if let CodexEvent::ThreadStarted { thread_id } = event {
                first(valid(thread_id));
            }
        }),
        Harness::CodexReview => false,
    };
    let id = found.flatten();
    assert!(id.as_ref().is_none_or(|id| !id.0.is_empty()));
    id
}

/// Whether the stream at `log` shows a run that ended before its first turn, as when a resume
/// names a session the harness cannot open; false when the log is unreadable.
pub fn ended_before_any_turn(harness: Harness, log: &Path) -> bool {
    if harness == Harness::Codex {
        // A refused `codex exec resume` prints only this error to stderr: no thread starts.
        let mut started = false;
        let read = scan(log, |event| {
            started |= matches!(event, CodexEvent::ThreadStarted { .. });
        });
        let mut head = String::new();
        let text = File::open(log).and_then(|file| {
            let bound = u64::try_from(CODEX_REFUSAL_BYTES_MAX).unwrap_or(u64::MAX);
            file.take(bound).read_to_string(&mut head)
        });
        let refused = text.is_ok() && head.contains(CODEX_NO_ROLLOUT);
        assert!(head.len() <= CODEX_REFUSAL_BYTES_MAX);
        return read && !started && refused;
    }
    let mut results = 0u64;
    let mut refused = false;
    let mut turned = false;
    let read = scan(log, |event| match event {
        ClaudeEvent::Result {
            is_error,
            num_turns,
            ..
        } => {
            results = results.saturating_add(1);
            refused = valid(is_error) == Some(true) && valid(num_turns) == Some(0);
        }
        ClaudeEvent::Assistant { .. } | ClaudeEvent::User { .. } => turned = true,
        ClaudeEvent::Other => {}
    });
    let before = read && results == 1 && refused && !turned;
    assert!(results <= LINES_MAX, "scan reads at most LINES_MAX events");
    before
}

pub fn stopped_by_content_filter(harness: &Harness, log: &Path) -> bool {
    let stopped = match harness {
        Harness::Claude => false,
        Harness::Codex => {
            let mut stopped = false;
            let _ = scan(log, |event| match event {
                CodexEvent::TurnFailed { error } => {
                    let message = valid(error).and_then(|error| valid(error.message));
                    stopped = message.is_some_and(|text| text.starts_with(CONTENT_FILTER));
                }
                CodexEvent::TurnCompleted { .. } => stopped = false,
                CodexEvent::ItemStarted { .. }
                | CodexEvent::ThreadStarted { .. }
                | CodexEvent::ItemCompleted { .. }
                | CodexEvent::Error { .. }
                | CodexEvent::Other => {}
            });
            stopped
        }
        // Codex prints its token-usage footer after the terminal error as it shuts down.
        Harness::CodexReview => stderr_tail(log).is_some_and(|text| {
            let lines = text.lines().rev().filter(|line| !line.trim().is_empty());
            let mut last = lines.take(STDERR_FINAL_LINES);
            last.any(|line| {
                let error = line.trim().strip_prefix("ERROR: ");
                error.is_some_and(|error| error.starts_with(CONTENT_FILTER))
            })
        }),
    };
    assert!(!stopped || *harness != Harness::Claude);
    stopped
}

fn stderr_tail(log: &Path) -> Option<String> {
    let mut file = File::open(stderr_log(log)).ok()?;
    let len = file.metadata().ok()?.len();
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL_BYTES)))
        .and_then(|_| file.take(STDERR_TAIL_BYTES).read_to_end(&mut tail))
        .ok()?;
    assert!(u64::try_from(tail.len()).is_ok_and(|n| n <= STDERR_TAIL_BYTES));
    Some(String::from_utf8_lossy(&tail).into_owned())
}

pub fn clip(text: &str, chars_max: usize, from_end: bool) -> String {
    let bidi = |c: char| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let shown: String = (text.chars())
        .map(|c| if c.is_control() || bidi(c) { ' ' } else { c })
        .collect();
    let line = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = line.chars().count();
    let clipped: String = if from_end {
        line.chars().skip(count.saturating_sub(chars_max)).collect()
    } else {
        line.chars().take(chars_max).collect()
    };
    assert!(clipped.chars().count() <= chars_max);
    assert!(!clipped.chars().any(|c| c.is_control() || bidi(c)));
    clipped
}

// What a stopped run did, from its own log: the commands it ran with their exit codes and a
// tail of their output, its last message, and what it was running when it stopped.
pub fn handoff(harness: &Harness, log: &Path) -> Vec<String> {
    let mut lines: VecDeque<String> = VecDeque::new();
    let mut push = |line: String| {
        if lines.len() == HANDOFF_LINES_MAX {
            lines.pop_front();
        }
        lines.push_back(line);
    };
    match harness {
        Harness::Claude => {}
        Harness::Codex => {
            let mut running: Option<String> = None;
            let _ = scan(log, |event| {
                let (started, item) = match event {
                    CodexEvent::ItemStarted { item } => (true, item),
                    CodexEvent::ItemCompleted { item } => (false, item),
                    _ => return,
                };
                let Some(item) = valid(item) else { return };
                let kind = valid(item.kind);
                let command = valid(item.command).map(|text| clip(&text, CLIP_CHARS, false));
                match (kind.as_deref(), command, started) {
                    (Some("command_execution"), Some(command), true) => running = Some(command),
                    (Some("command_execution"), Some(command), false) => {
                        running = None;
                        let code = valid(item.exit_code)
                            .map_or_else(|| "none".to_string(), |code| code.to_string());
                        let output = valid(item.aggregated_output).unwrap_or_default();
                        let output = clip(&output, CLIP_CHARS, true);
                        push(format!("- ran `{command}`, exit {code}: {output}"));
                    }
                    (Some("agent_message"), _, false) => {
                        let text = valid(item.text).unwrap_or_default();
                        push(format!("- said: {}", clip(&text, CLIP_CHARS, false)));
                    }
                    _ => {}
                }
            });
            if let Some(command) = running {
                push(format!("- was running `{command}` when it stopped"));
            }
        }
        Harness::CodexReview => {
            let tail = stderr_tail(log).unwrap_or_default();
            let kept = tail.lines().filter(|line| !line.trim().is_empty());
            kept.for_each(|line| push(format!("    {}", clip(line, CLIP_CHARS, false))));
        }
    }
    let lines: Vec<String> = lines.into();
    assert!(lines.len() <= HANDOFF_LINES_MAX);
    assert!(
        lines
            .iter()
            .all(|line| line.chars().count() <= HANDOFF_LOG_LINE_CHARS_MAX)
    );
    lines
}

/// Whether the stream at `log` records a failed run; false when the log is unreadable.
pub fn stream_reports_failure(harness: &Harness, log: &Path) -> bool {
    let mut last_result_failed = false;
    let mut last_turn_failed = false;
    let mut last_review_failed = false;
    // A log cut short still counts for what it showed.
    let _ = match harness {
        // `item.type == "error"` inside `item.completed` is a harmless warning, and a
        // top-level `error` may be a reconnect attempt that a later `turn.completed` clears.
        Harness::Codex => scan(log, |event| match event {
            CodexEvent::TurnFailed { .. } | CodexEvent::Error { .. } => last_turn_failed = true,
            CodexEvent::TurnCompleted { .. } => last_turn_failed = false,
            CodexEvent::ThreadStarted { .. }
            | CodexEvent::ItemStarted { .. }
            | CodexEvent::ItemCompleted { .. }
            | CodexEvent::Other => {}
        }),
        // Without an event stream, a run that exits 0 still failed when it printed no review or
        // Codex's own message for a review delegate that never started.
        Harness::CodexReview => {
            last_review_failed =
                last_message(harness, log).is_none_or(|text| text.starts_with(REVIEW_INTERRUPTED));
            true
        }
        Harness::Claude => scan(log, |event| {
            if let ClaudeEvent::Result {
                subtype, is_error, ..
            } = event
            {
                let subtype = valid(subtype);
                last_result_failed =
                    valid(is_error) == Some(true) || subtype.as_deref() != Some("success");
            }
        }),
    };
    assert!(!last_result_failed || *harness == Harness::Claude);
    assert!(!last_turn_failed || *harness == Harness::Codex);
    assert!(!last_review_failed || *harness == Harness::CodexReview);
    last_result_failed || last_turn_failed || last_review_failed
}

pub const TOKENS_MAX: u64 = 1 << 40;
pub const COST_MICROS_MAX: u64 = 1 << 40;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost_micros: u64,
    pub unpriced: bool,
}

impl Usage {
    pub fn plus(self, other: Usage) -> Usage {
        let sum = Usage {
            input: self.input.saturating_add(other.input),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
            output: self.output.saturating_add(other.output),
            cost_micros: self.cost_micros.saturating_add(other.cost_micros),
            unpriced: self.unpriced || other.unpriced,
        };
        assert!(sum.input >= self.input && sum.output >= other.output);
        sum
    }
}

// Thousandths of a US dollar per million tokens: input, cache read, cache write, output.
// Source: https://developers.openai.com/api/docs/pricing, Standard tier, prompts up to 272K
// tokens, read 2026-11-20. Codex reports one cache-write count without its tier, so writes are
// priced at the 30-minute rate, the lower of the two. Claude reports its own cost.
const PRICES: [(&str, [u64; 4]); 2] = [
    ("gpt-6.1-sol", [2_000, 100, 2_500, 10_000]),
    ("gpt-6.2-sol", [2_000, 100, 2_500, 10_000]),
];

fn priced(model: &str, usage: &Usage) -> Option<u64> {
    let (_, price) = PRICES.iter().find(|(name, _)| *name == model)?;
    let tokens = [
        usage.input,
        usage.cache_read,
        usage.cache_write,
        usage.output,
    ];
    let mut nanos: u64 = 0;
    for (count, rate) in tokens.iter().zip(price) {
        nanos = nanos.checked_add(count.checked_mul(*rate)?)?;
    }
    let micros = nanos / 1000;
    assert!(micros <= nanos);
    (micros <= COST_MICROS_MAX).then_some(micros)
}

fn micros_of(dollars: f64) -> Option<u64> {
    let micros = (dollars * 1e6).round();
    let bound = COST_MICROS_MAX as f64;
    let valid = micros.is_finite() && (0.0..=bound).contains(&micros);
    let micros = valid.then_some(micros as u64);
    assert!(micros.is_none_or(|micros| micros <= COST_MICROS_MAX));
    micros
}

pub fn usage(harness: &Harness, model: &str, log: &Path) -> Option<Usage> {
    assert!(!model.is_empty());
    let mut found: Option<Usage> = None;
    let mut reported: Option<u64> = None;
    let complete = match harness {
        Harness::Claude => scan(log, |event| {
            if let ClaudeUsageEvent::Result {
                usage,
                total_cost_usd,
            } = event
                && let Some(usage) = valid(usage)
            {
                found = Some(Usage {
                    input: usage.input_tokens,
                    cache_read: usage.cache_read_input_tokens,
                    cache_write: usage.cache_creation_input_tokens,
                    output: usage.output_tokens,
                    ..Usage::default()
                });
                reported = valid(total_cost_usd).and_then(micros_of);
            }
        }),
        // Each `turn.completed` carries the run's running totals, not the turn's own.
        Harness::Codex => scan(log, |event| {
            let CodexEvent::TurnCompleted { usage } = event else {
                return;
            };
            let snapshot = valid(usage).and_then(|turn| {
                let cached = turn.cached_input_tokens;
                let write = turn.cache_write_input_tokens;
                let input = turn.input_tokens.checked_sub(cached)?.checked_sub(write)?;
                Some(Usage {
                    input,
                    cache_read: cached,
                    cache_write: write,
                    output: turn.output_tokens,
                    ..Usage::default()
                })
            });
            found = snapshot;
        }),
        // `codex review` prints plain text and reports no usage.
        Harness::CodexReview => return None,
    };
    let mut usage = found.filter(|_| complete)?;
    let counts = [
        usage.input,
        usage.cache_read,
        usage.cache_write,
        usage.output,
    ];
    if counts.iter().any(|count| *count > TOKENS_MAX) {
        return None;
    }
    match reported.or_else(|| priced(model, &usage)) {
        Some(micros) => usage.cost_micros = micros,
        None => usage.unpriced = true,
    }
    assert!(!usage.unpriced || usage.cost_micros == 0);
    Some(usage)
}

pub fn failure_account(harness: &Harness, log: &Path, tail: &str) -> Option<String> {
    let mut account = Account::default();
    let _ = match harness {
        Harness::Claude => scan(log, |event| account.claude(event)),
        Harness::Codex => scan(log, |event| account.codex(event)),
        Harness::CodexReview => return None,
    };
    let mut text = String::new();
    if !account.messages.is_empty() {
        text.push_str("The agent's last messages:");
        account
            .messages
            .iter()
            .for_each(|message| quote(&mut text, message));
    }
    let ends = [
        ("Its last failed tool call: ", &account.tool),
        ("Its run ended in error: ", &account.error),
    ];
    for (lead, end) in ends {
        if let Some(end) = end {
            text.push_str(if text.is_empty() { "" } else { "\n" });
            text.push_str(lead);
            text.push_str(&end.replace('\n', " "));
        }
    }
    if text.is_empty() {
        return None;
    }
    let plain = tail.lines().filter(|line| {
        !line.trim().is_empty() && serde_json::from_str::<IgnoredAny>(line).is_err()
    });
    let plain: Vec<&str> = plain.collect();
    let plain = &plain[plain.len().saturating_sub(ACCOUNT_PLAIN_LINES_MAX)..];
    if !plain.is_empty() {
        text.push_str("\nThe log's last lines that are not events:");
        let plain = plain
            .iter()
            .filter_map(|line| piece(Some(line.to_string())));
        plain.for_each(|line| quote(&mut text, &line));
    }
    assert!(account.messages.len() <= ACCOUNT_MESSAGES_MAX);
    assert!(!text.is_empty());
    Some(text)
}

fn quote(text: &mut String, piece: &str) {
    assert!(!piece.is_empty());
    for line in piece.lines() {
        text.push_str("\n> ");
        text.push_str(line.trim_end());
    }
    assert!(text.ends_with(piece.lines().last().unwrap_or_default().trim_end()));
}

const ACCOUNT_PLAIN_LINES_MAX: usize = 5;
const ACCOUNT_MESSAGES_MAX: usize = 3;
const ACCOUNT_PIECE_CHARS_MAX: usize = 2_000;

#[derive(Default)]
struct Account {
    messages: Vec<String>,
    tool: Option<String>,
    error: Option<String>,
}

impl Account {
    fn said(&mut self, text: Option<String>) {
        let Some(text) = piece(text) else {
            return;
        };
        if self.messages.len() == ACCOUNT_MESSAGES_MAX {
            self.messages.remove(0);
        }
        self.messages.push(text);
        assert!(self.messages.len() <= ACCOUNT_MESSAGES_MAX);
    }

    fn claude(&mut self, event: ClaudeEvent) {
        assert!(self.messages.len() <= ACCOUNT_MESSAGES_MAX);
        match event {
            ClaudeEvent::Assistant { message } => {
                for block in blocks(message) {
                    if valid(block.kind).as_deref() == Some("text") {
                        self.said(valid(block.text));
                    }
                }
            }
            ClaudeEvent::User { message } => {
                for block in blocks(message) {
                    if valid(block.is_error) != Some(true) {
                        continue;
                    }
                    let text = match valid(block.content) {
                        Some(ToolContent::Text(text)) => Some(text),
                        Some(ToolContent::Blocks(inner)) => {
                            let texts = inner.into_iter().filter_map(|b| valid(Some(b)));
                            let texts: Vec<String> = texts.filter_map(|b| valid(b.text)).collect();
                            Some(texts.join("\n"))
                        }
                        None => None,
                    };
                    self.tool = piece(text).or_else(|| Some("(no text)".to_string()));
                }
            }
            ClaudeEvent::Result {
                result,
                errors,
                subtype,
                is_error,
                ..
            } => {
                let failed =
                    valid(is_error) == Some(true) || valid(subtype).as_deref() != Some("success");
                let errors = valid(errors).and_then(|errors| errors.into_iter().last());
                let text = piece(valid(result)).or_else(|| piece(errors));
                self.error = failed.then_some(text).flatten();
            }
            ClaudeEvent::Other => {}
        }
    }

    fn codex(&mut self, event: CodexEvent) {
        assert!(self.messages.len() <= ACCOUNT_MESSAGES_MAX);
        match event {
            CodexEvent::ItemCompleted { item } => {
                let Some(item) = valid(item) else {
                    return;
                };
                match valid(item.kind).as_deref() {
                    Some("agent_message") => self.said(valid(item.text)),
                    Some("command_execution") => {
                        let code = valid(item.exit_code).filter(|code| *code != 0);
                        let command = piece(valid(item.command));
                        if let (Some(code), Some(command)) = (code, command) {
                            self.tool = Some(format!("`{command}` exited {code}"));
                        }
                    }
                    _ => {}
                }
            }
            CodexEvent::TurnFailed { error } => {
                let message = valid(error).and_then(|error| valid(error.message));
                self.error = piece(message).or_else(|| Some("(no message)".to_string()));
            }
            CodexEvent::Error { message } => self.error = piece(valid(message)),
            CodexEvent::TurnCompleted { .. } => self.error = None,
            CodexEvent::ItemStarted { .. }
            | CodexEvent::ThreadStarted { .. }
            | CodexEvent::Other => {}
        }
    }
}

fn blocks(message: Option<Field<ClaudeMessage>>) -> impl Iterator<Item = ClaudeBlock> {
    let content = valid(message).and_then(|message| valid(message.content));
    content
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| valid(Some(b)))
}

fn piece(text: Option<String>) -> Option<String> {
    let text: String = text?.chars().map(shown).collect();
    let text = message_tail(text.trim(), ACCOUNT_PIECE_CHARS_MAX).trim();
    assert!(text.chars().count() <= ACCOUNT_PIECE_CHARS_MAX);
    assert!(!text.contains(|c: char| c != '\n' && shown(c) != c));
    (!text.is_empty()).then(|| text.to_string())
}

// Terminals act on control characters, and bidi overrides reorder the text a reader sees.
fn shown(c: char) -> char {
    let bidi =
        matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    if c != '\n' && (c.is_control() || bidi) {
        ' '
    } else {
        c
    }
}
