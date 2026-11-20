//! Launches one agent, records its output to a size-capped log, stops it at a time limit, and
//! reads its final message back from the recorded stream.

use crate::board::one_line;
use crate::roles::{AGENTS_MAX, Harness, RoleSpec};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::process::Signal;
use serde_json::Value;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
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

/// The command line that runs `spec` on `prompt` in `dir`; the prompt is last, after `--`.
pub fn command_for(spec: &RoleSpec, prompt: &str, dir: &Path) -> Result<CommandLine, HarnessError> {
    assert!(!spec.model.is_empty() && !spec.model.starts_with('-'));
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
    let (program, head) = match spec.harness {
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
            ],
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
            ],
        ),
        Harness::Tact => ("tact", vec!["--model", model, "--workspace", &dir, "run"]),
    };
    let mut args: Vec<String> = head.iter().map(|a| a.to_string()).collect();
    args.push("--".to_string());
    args.push(prompt.to_string());
    assert!(args.len() <= ARGS_MAX);
    assert!(args.iter().any(|a| a == model));
    let program = program.to_string();
    let env = Vec::new();
    Ok(CommandLine { program, args, env })
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
    shared: &Shared,
    seconds_max: u64,
) -> Result<(ExitStatus, bool), HarnessError> {
    assert!(seconds_max > 0 && seconds_max <= ATTEMPT_SECONDS_MAX);
    assert!(child.id() > 1, "the group of a child rider started");
    let start = (Instant::now(), SystemTime::now());
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
        if let Some(message) = shared.error() {
            let _ = kill_and_reap(child);
            return Err(HarnessError::Log(message));
        }
        let now = (Instant::now(), SystemTime::now());
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawned = CommandWrap::from(process)
        .wrap(ProcessGroup::leader())
        .spawn();
    spawned.map_err(|error| HarnessError::Spawn(format!("{}: {error}", cmd.program)))
}

fn start_readers(
    child: &mut dyn ChildWrapper,
    slots: [ReaderSlot; READERS],
    shared: &Arc<Shared>,
    done: &SyncSender<Result<(), String>>,
) -> Result<(), HarnessError> {
    let missing = |name: &str| HarnessError::Spawn(format!("the agent has no {name}"));
    let stdout = child.stdout().take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr().take().ok_or_else(|| missing("stderr"))?;
    let [stdout_slot, stderr_slot] = slots;
    start_reader(stdout, "stdout", stdout_slot, shared, done)?;
    start_reader(stderr, "stderr", stderr_slot, shared, done)?;
    assert!(child.stdout().is_none() && child.stderr().is_none());
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
    assert!(seconds_max > 0 && seconds_max <= ATTEMPT_SECONDS_MAX);
    assert!(log_bytes_max <= LOG_BYTES_MAX);
    assert!(cmd.args.len() <= ARGS_MAX && cmd.env.len() <= ENV_MAX);
    let reserve = || {
        let slot = ReaderSlot::reserve(&READER_THREADS, READER_THREADS_MAX);
        slot.ok_or(HarnessError::ReadersExhausted)
    };
    let slots = [reserve()?, reserve()?];
    let shared = open_log(log, log_bytes_max)?;
    let mut child = spawn(cmd, dir)?;
    assert!(
        child.id() > 1,
        "a spawned child is never init or the scheduler"
    );
    let (done, finished) = mpsc::sync_channel(READERS);
    if let Err(error) = start_readers(child.as_mut(), slots, &shared, &done) {
        let _ = kill_and_reap(child.as_mut());
        shared.close();
        return Err(error);
    }
    drop(done);
    let outcome = supervise(child.as_mut(), &shared, seconds_max);
    // The leader is reaped; leftover members of its group would hold the pipes open.
    let _ = child.start_kill();
    let drained = drain(&finished);
    let logged = shared.close();
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

// Visits each line of `log` that is JSON; skips other lines and those over `LINE_BYTES_MAX`.
// Returns whether the whole log was read.
fn scan(log: &Path, mut visit: impl FnMut(&Value)) -> bool {
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
        if let Ok(event) = serde_json::from_slice::<Value>(&line) {
            visit(&event);
        }
    }
    false
}

fn text_at<'a>(event: &'a Value, pointer: &str) -> Option<&'a str> {
    assert!(pointer.starts_with('/'));
    let text = event.pointer(pointer).and_then(Value::as_str);
    assert!(text.is_none_or(|t| t.len() <= LINE_BYTES_MAX));
    text
}

// `Some(text)` when `event` is where `harness` puts its final message, `text` being `None`
// when that event carries no string; `None` for any other event.
fn final_text<'a>(harness: &Harness, event: &'a Value) -> Option<Option<&'a str>> {
    let kind = text_at(event, "/type")?;
    let found = match harness {
        Harness::Claude => (kind == "result").then(|| text_at(event, "/result")),
        Harness::Codex => (kind == "item.completed"
            && text_at(event, "/item/type") == Some("agent_message"))
        .then(|| text_at(event, "/item/text")),
        Harness::Tact => (kind == "assistant.message"
            && text_at(event, "/payload/phase") == Some("final_answer"))
        .then(|| text_at(event, "/payload/text")),
    };
    assert!(found.flatten().is_none_or(|t| t.len() <= LINE_BYTES_MAX));
    found
}

/// The agent's trimmed final message, or `None` when the log is unreadable or has none.
pub fn last_message(harness: &Harness, log: &Path) -> Option<String> {
    let mut last: Option<String> = None;
    let complete = scan(log, |event| {
        if let Some(text) = final_text(harness, event) {
            last = text.map(str::to_string);
        }
    });
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

/// Whether the stream at `log` records a failed run; false when the log is unreadable.
pub fn stream_reports_failure(harness: &Harness, log: &Path) -> bool {
    let mut failed = false;
    let mut last_result_failed = false;
    // A log cut short still counts for what it showed.
    let _ = scan(log, |event| {
        let kind = text_at(event, "/type");
        match harness {
            // tact exits 0 when cancelled; only the stream says so.
            Harness::Tact => failed |= kind == Some("run.failed"),
            // `item.type == "error"` inside `item.completed` is a harmless warning.
            Harness::Codex => failed |= matches!(kind, Some("turn.failed" | "error")),
            Harness::Claude if kind == Some("result") => {
                let is_error = event.pointer("/is_error").and_then(Value::as_bool);
                let subtype = text_at(event, "/subtype");
                last_result_failed = is_error == Some(true) || subtype != Some("success");
            }
            Harness::Claude => {}
        }
    });
    assert!(!failed || *harness != Harness::Claude);
    assert!(!last_result_failed || *harness == Harness::Claude);
    failed || last_result_failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::role_from_label;
    use std::path::PathBuf;

    const TACT_ROLE: RoleSpec = RoleSpec {
        label: "tact-test",
        harness: Harness::Tact,
        model: "sol",
        commits_expected: false,
        needs_of: true,
        title: "Tact-test",
        model_name: "Sol",
        model_slug: "sol",
        slots: 1,
        pool_slots: 1,
        prompt_file: "none.md",
    };

    const PROMPT: &str = "Do the task.";

    fn scratch_file(name: &str, text: &str) -> PathBuf {
        let unique = format!("rider-harness-unit-{name}-{}", std::process::id());
        let path = std::env::temp_dir().join(unique);
        fs::write(&path, text).unwrap();
        path
    }

    fn reports_failure(name: &str, harness: Harness, text: &str) -> bool {
        let log = scratch_file(name, text);
        let failed = stream_reports_failure(&harness, &log);
        fs::remove_file(&log).unwrap();
        failed
    }

    #[test]
    fn claude_command_matches_the_recorded_form() {
        let spec = role_from_label("implement").unwrap();
        let c = command_for(spec, PROMPT, Path::new("/w")).unwrap();
        assert_eq!(c.program, "claude");
        let expected = [
            "-p",
            "--model",
            "claude-opus-5-5",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--",
            PROMPT,
        ];
        assert_eq!(c.args, expected);
        assert_eq!(c.env, []);
    }

    #[test]
    fn codex_command_matches_the_recorded_form() {
        let spec = role_from_label("review").unwrap();
        let c = command_for(spec, PROMPT, Path::new("/w")).unwrap();
        assert_eq!(c.program, "codex");
        let expected = [
            "exec",
            "--model",
            "gpt-6.1-sol",
            "--json",
            "--dangerously-bypass-approvals-and-sandbox",
            "-C",
            "/w",
            "--",
            PROMPT,
        ];
        assert_eq!(c.args, expected);
        assert_eq!(c.env, []);
    }

    #[test]
    fn tact_command_matches_the_recorded_form() {
        let c = command_for(&TACT_ROLE, PROMPT, Path::new("/w")).unwrap();
        assert_eq!(c.program, "tact");
        let expected = ["--model", "sol", "--workspace", "/w", "run", "--", PROMPT];
        assert_eq!(c.args, expected);
        assert_eq!(c.env, []);
    }

    #[test]
    fn a_prompt_is_one_argument_after_the_options() {
        let prompt = "--help me\nplease";
        let security = role_from_label("security").unwrap();
        for spec in [security, &TACT_ROLE] {
            let c = command_for(spec, prompt, Path::new("/w")).unwrap();
            assert_eq!(c.args[c.args.len() - 2..], ["--", prompt]);
        }
        let mythos = command_for(security, PROMPT, Path::new("/w")).unwrap();
        assert_eq!(mythos.args[1..3], ["--model", "claude-mythos-5-2"]);
    }

    #[test]
    fn a_prompt_a_cli_could_misread_is_an_error() {
        let claude = role_from_label("implement").unwrap();
        let codex = role_from_label("review").unwrap();
        for prompt in ["", " \n\t ", "-", "mcp", "doctor", "--help"] {
            for spec in [claude, codex, &TACT_ROLE] {
                let result = command_for(spec, prompt, Path::new("/w"));
                assert!(
                    matches!(result, Err(HarnessError::PromptUnsafe)),
                    "{prompt:?}"
                );
            }
        }
    }

    #[test]
    fn an_oversized_prompt_is_an_error() {
        let spec = role_from_label("implement").unwrap();
        let prompt = "x ".repeat(PROMPT_BYTES_MAX / 2);
        assert_eq!(prompt.len(), PROMPT_BYTES_MAX);
        let c = command_for(spec, &prompt, Path::new("/w")).unwrap();
        assert_eq!(c.args.last(), Some(&prompt));
        let over = format!("{prompt}x");
        let result = command_for(spec, &over, Path::new("/w"));
        assert!(matches!(
            result,
            Err(HarnessError::PromptTooLarge { bytes }) if bytes == PROMPT_BYTES_MAX + 1
        ));
    }

    #[test]
    fn either_clock_can_end_an_attempt() {
        let limit = Duration::from_secs(60);
        let (under, over) = (Duration::from_secs(59), Duration::from_secs(61));
        let backwards = SystemTime::UNIX_EPOCH.duration_since(SystemTime::now());
        assert!(backwards.is_err());
        assert!(!timed_out(under, Ok(under), limit));
        assert!(timed_out(limit, Ok(under), limit));
        assert!(timed_out(over, Ok(under), limit));
        assert!(timed_out(under, Ok(limit), limit), "the machine slept");
        assert!(timed_out(under, Ok(Duration::from_secs(3600)), limit));
        assert!(
            !timed_out(under, backwards.clone(), limit),
            "the clock went back"
        );
        assert!(timed_out(over, backwards, limit));
    }

    #[test]
    fn checking_for_a_log_error_never_waits_on_a_write() {
        let path = scratch_file("lock", "");
        let shared = open_log(&path, 16).unwrap();
        let held = shared.file.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let probe = Arc::clone(&shared);
        thread::spawn(move || {
            probe.fail("disk full".to_string());
            tx.send(probe.error()).unwrap();
        });
        let answer = rx.recv_timeout(Duration::from_secs(1));
        drop(held);
        assert_eq!(answer, Ok(Some("disk full".to_string())));
        assert_eq!(shared.close(), Some("disk full".to_string()));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_log_takes_bytes_up_to_its_cap() {
        let path = scratch_file("cap", "");
        let shared = open_log(&path, 5).unwrap();
        assert_eq!(shared.accept(b"abc"), Ok(()));
        assert_eq!(shared.accept(b"defg"), Ok(()));
        assert_eq!(shared.accept(b"h"), Ok(()));
        assert_eq!(shared.close(), None);
        assert_eq!(shared.accept(b"i"), Ok(()));
        assert_eq!(fs::read(&path).unwrap(), b"abcde");
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reader_slots_are_bounded_and_returned() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let first = ReaderSlot::reserve(&LIVE, 2).unwrap();
        let second = ReaderSlot::reserve(&LIVE, 2).unwrap();
        assert!(ReaderSlot::reserve(&LIVE, 2).is_none());
        assert_eq!(LIVE.load(Ordering::SeqCst), 2);
        drop(first);
        let third = ReaderSlot::reserve(&LIVE, 2).unwrap();
        drop((second, third));
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn last_message_is_found_in_each_recorded_stream() {
        let cases = [
            (Harness::Claude, "claude"),
            (Harness::Codex, "codex"),
            (Harness::Tact, "tact"),
        ];
        for (h, f) in cases {
            let path = format!("fixtures/harness/{f}.jsonl");
            let m = last_message(&h, Path::new(&path));
            assert_eq!(m.as_deref(), Some("DONE"), "{f}");
        }
    }

    #[test]
    fn last_message_survives_hostile_logs() {
        let missing = Path::new("/nonexistent/rider/agent.log");
        assert_eq!(last_message(&Harness::Claude, missing), None);
        let huge = "y".repeat(LINE_BYTES_MAX + 10);
        let events = r#"not json
[1,2]
"str"
{"type":"result","result":7}
{"type":"result","result":"  first  "}
"#;
        let text = format!("{huge}\n{events}{huge}");
        let log = scratch_file("hostile", &text);
        let found = last_message(&Harness::Claude, &log);
        assert_eq!(found.as_deref(), Some("first"));
        assert_eq!(last_message(&Harness::Codex, &log), None);
        fs::remove_file(&log).unwrap();
        let blank = r#"{"type":"result","result":" \n "}"#;
        let log = scratch_file("blank", blank);
        assert_eq!(last_message(&Harness::Claude, &log), None);
        fs::remove_file(&log).unwrap();
    }

    #[test]
    fn recorded_streams_report_no_failure() {
        let cases = [
            (Harness::Claude, "claude"),
            (Harness::Codex, "codex"),
            (Harness::Tact, "tact"),
        ];
        for (h, f) in cases {
            let path = format!("fixtures/harness/{f}.jsonl");
            assert!(!stream_reports_failure(&h, Path::new(&path)), "{f}");
        }
        let missing = Path::new("/nonexistent/rider/agent.log");
        assert!(!stream_reports_failure(&Harness::Tact, missing));
    }

    #[test]
    fn failures_in_the_stream_are_reported() {
        let tact = r#"{"type":"run.started"}
{"type":"run.failed","payload":{"status":"cancelled"}}
"#;
        assert!(reports_failure("tact-failed", Harness::Tact, tact));
        let claude = r#"{"type":"result","subtype":"success","is_error":true}"#;
        assert!(reports_failure("claude-error", Harness::Claude, claude));
        let claude = r#"{"type":"result","subtype":"error_max_turns","is_error":false}"#;
        assert!(reports_failure("claude-subtype", Harness::Claude, claude));
        let codex = r#"{"type":"item.completed","item":{"type":"error"}}"#;
        assert!(!reports_failure("codex-warning", Harness::Codex, codex));
        let codex = r#"{"type":"turn.failed","error":{"message":"x"}}"#;
        assert!(reports_failure("codex-failed", Harness::Codex, codex));
        let codex = r#"{"type":"error","message":"x"}"#;
        assert!(reports_failure("codex-error", Harness::Codex, codex));
    }
}
