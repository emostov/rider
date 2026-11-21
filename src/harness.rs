//! Launches one agent, records its output to a size-capped log, stops it at a time limit, and
//! reads its final message back from the recorded stream.

use crate::board::one_line;
use crate::git;
use crate::roles::{AGENTS_MAX, Harness, RoleSpec};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::process::Signal;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
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
const ARGS_MAX: usize = 16;
const ENV_MAX: usize = 16;
const POLL_MILLIS: u64 = 50;
// Every poll sleeps `POLL_MILLIS`, so the final kill comes before the last poll.
const POLLS_MAX: u64 = (ATTEMPT_SECONDS_MAX + KILL_GRACE_SECONDS) * 1000 / POLL_MILLIS + 16;
const CHUNK_BYTES: usize = 64 * 1024;
const READS_MAX: u64 = 1 << 32;
const READERS: usize = 2;
const DRAIN_SECONDS_MAX: u64 = 5;
// Reader threads alive across all runs: two per running agent, plus room for readers left
// blocked on a pipe that a process outside the agent's group still holds open.
const READER_THREADS_MAX: usize = 4 * READERS * AGENTS_MAX as usize;
const LINE_BYTES_MAX: usize = 4 * 1024 * 1024;
// Every line holds at least its newline, so a log read up to `LOG_BYTES_MAX` has fewer lines.
const LINES_MAX: u64 = LOG_BYTES_MAX + 1;
const SKIP_READS_MAX: u64 = LOG_BYTES_MAX / LINE_BYTES_MAX as u64 + 2;
const REVIEW_INTERRUPTED: &str = "Review was interrupted.";
// Codex's content-filter error has no code: its message's first sentence names it.
const CONTENT_FILTER: &str = "This content was flagged for possible cybersecurity risk.";
const STDERR_TAIL_BYTES: u64 = 64 * 1024;
const POISONED: &str = "the log lock was poisoned";

const _: () = {
    assert!(LINES_MAX > LOG_BYTES_MAX);
    assert!(SKIP_READS_MAX * LINE_BYTES_MAX as u64 > LOG_BYTES_MAX);
    assert!(DRAIN_SECONDS_MAX > 0 && POLL_MILLIS > 0);
    assert!(POLLS_MAX * POLL_MILLIS > (ATTEMPT_SECONDS_MAX + KILL_GRACE_SECONDS) * 1000);
    assert!(READER_THREADS_MAX > READERS * AGENTS_MAX as usize);
};

static READER_THREADS: AtomicUsize = AtomicUsize::new(0);

/// A program and its arguments, ready to run in a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub program: String,
    pub args: Vec<String>,
    /// Set on top of the inherited environment.
    pub env: Vec<(String, String)>,
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
        }
    }
}

/// Upper bound on a session id's length.
pub const SESSION_BYTES_MAX: usize = 128;

/// A Claude Code session id: ASCII letters, digits, and `-`, never starting with `-`.
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

/// The command line that runs `spec` on `prompt` in `dir`, starting from a fork of session
/// `resume` when given (Claude only); the prompt is last, after `--`.
pub fn command_for(
    spec: &RoleSpec,
    prompt: &str,
    dir: &Path,
    resume: Option<&SessionId>,
) -> Result<CommandLine, HarnessError> {
    assert!(!spec.model.is_empty() && !spec.model.starts_with('-'));
    assert!(
        resume.is_none() || spec.harness == Harness::Claude,
        "only Claude sessions are resumed"
    );
    assert!(dir.is_absolute(), "agents run in absolute worktrees");
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
    let (program, head, io) = match spec.harness {
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
                "--",
                prompt,
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
                "-C",
                &dir,
                "--",
                prompt,
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
                "-",
            ],
            Io::PromptOnStdin(prompt.to_string()),
        ),
    };
    let mut args: Vec<String> = head.iter().map(|a| a.to_string()).collect();
    if let Some(session) = resume {
        // Without `--fork-session` Claude appends to the resumed session in place.
        let at = args.len() - 2;
        let fork = [session.0.clone(), "--fork-session".to_string()];
        args.splice(at..at, ["--resume".to_string()].into_iter().chain(fork));
    }
    assert!(args.len() <= ARGS_MAX);
    assert!(args.iter().any(|a| a == model || *a == model_setting));
    let program = program.to_string();
    let env = Vec::new();
    Ok(CommandLine {
        program,
        args,
        env,
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

fn open_log(log: &Path, log_bytes_max: u64) -> Result<Arc<Shared>, HarnessError> {
    assert!(!log.as_os_str().is_empty());
    assert!(log_bytes_max <= LOG_BYTES_MAX);
    let failed = |error: io::Error| HarnessError::Log(format!("{}: {error}", log.display()));
    if let Some(parent) = log.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(failed)?;
    }
    let file = File::create(log).map_err(failed)?;
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

fn feed(child: &mut dyn ChildWrapper, io: &Io) -> Result<(), HarnessError> {
    let Io::PromptOnStdin(prompt) = io else {
        assert!(child.stdin().is_none());
        return Ok(());
    };
    assert!(prompt.len() <= PROMPT_BYTES_MAX);
    let missing = || HarnessError::Spawn("the agent has no stdin".to_string());
    let mut stdin = child.stdin().take().ok_or_else(missing)?;
    let prompt = prompt.clone();
    // An agent that exits without reading closes the pipe, so the write ends; its exit status
    // says what happened.
    let builder = thread::Builder::new().name("agent-stdin".to_string());
    let started = builder.spawn(move || {
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
    if let Err(error) = started.and_then(|()| feed(child.as_mut(), &cmd.io)) {
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
        subtype: Option<Field<String>>,
        is_error: Option<Field<bool>>,
        num_turns: Option<Field<u64>>,
    },
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "user")]
    User,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SessionLine {
    session_id: Option<Field<String>>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "item.completed")]
    ItemCompleted { item: Option<Field<CodexItem>> },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: Option<Field<CodexError>> },
    #[serde(rename = "turn.completed")]
    TurnCompleted,
    #[serde(rename = "error")]
    Error,
    #[serde(other)]
    Other,
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

/// The session id of the first Claude event that carries one, or `None` when there is none.
pub fn session_id(log: &Path) -> Option<SessionId> {
    let mut found: Option<Option<SessionId>> = None;
    let _ = scan(log, |line: SessionLine| {
        if found.is_none() {
            found = valid(line.session_id).map(|id| SessionId::parse(&id));
        }
    });
    let id = found.flatten();
    assert!(id.as_ref().is_none_or(|id| !id.0.is_empty()));
    id
}

/// Whether the Claude stream at `log` shows a run that ended in error before its first turn,
/// as when `--resume` names a session Claude cannot open; false when the log is unreadable.
pub fn ended_before_any_turn(log: &Path) -> bool {
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
        ClaudeEvent::Assistant | ClaudeEvent::User => turned = true,
        ClaudeEvent::Other => {}
    });
    let before = read && results == 1 && refused && !turned;
    assert!(results <= LINES_MAX, "scan reads at most LINES_MAX events");
    before
}

/// Whether the provider's content filter ended the run at `log`: for Codex, its last turn event
/// is a `turn.failed` whose error is the filter's; for `codex review`, its stderr transcript
/// ends with that error.
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
                CodexEvent::TurnCompleted => stopped = false,
                CodexEvent::ItemCompleted { .. } | CodexEvent::Error | CodexEvent::Other => {}
            });
            stopped
        }
        Harness::CodexReview => {
            let stderr = stderr_log(log);
            let Ok(mut file) = File::open(&stderr) else {
                return false;
            };
            let len = file.metadata().map_or(0, |metadata| metadata.len());
            let mut tail = Vec::new();
            let read = file
                .seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL_BYTES)))
                .and_then(|_| file.take(STDERR_TAIL_BYTES).read_to_end(&mut tail));
            let text = String::from_utf8_lossy(&tail);
            let last = text.lines().rev().find(|line| !line.trim().is_empty());
            let error = last.and_then(|line| line.trim().strip_prefix("ERROR: "));
            read.is_ok() && error.is_some_and(|text| text.starts_with(CONTENT_FILTER))
        }
    };
    assert!(!stopped || *harness != Harness::Claude);
    stopped
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
            CodexEvent::TurnFailed { .. } | CodexEvent::Error => last_turn_failed = true,
            CodexEvent::TurnCompleted => last_turn_failed = false,
            CodexEvent::ItemCompleted { .. } | CodexEvent::Other => {}
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
