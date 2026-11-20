//! The Board trait, and its one implementation over the `vb` command-line program.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Command as Process, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process_group};
use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer};

use crate::core::TASKS_MAX;
use crate::feed::Cursor;

/// A task's ID on the board. The board prints IDs as decimal strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Waiting,
    Ready,
    Claimed,
    Blocked,
    Completed,
    Failed,
    Canceled,
}

/// Upper bound on the labels of one task, the board's own `board.task` label included.
pub const LABELS_MAX: usize = 32;
/// Upper bound on what one `vb` run may print to stdout, and separately to stderr.
pub const OUTPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Upper bound on how long one claim waits for a ready task.
pub const WAIT_SECONDS_MAX: u32 = 3600;
/// Upper bound, in characters, on a task's result or a failure or block reason.
pub const RESULT_CHARS_MAX: usize = 500;
/// The most characters the board accepts in one message's text; it rejects anything longer.
pub const MESSAGE_CHARS_MAX: usize = 10_000;
/// Upper bound on how long one `vb` run may take, beyond any wait a claim asks for.
pub const BOARD_CALL_SECONDS_MAX: u32 = 30;
/// The limit actually applied; unit tests shorten it so a timeout can be observed quickly.
const CALL_SECONDS: u32 = if cfg!(test) {
    2
} else {
    BOARD_CALL_SECONDS_MAX
};
/// How long a killed `vb` group's pipes may take to close before their readers are abandoned.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How often a running `vb` is checked for having exited.
const POLL_MILLIS: u64 = 20;
const POLL_INTERVAL: Duration = Duration::from_millis(POLL_MILLIS);
/// Upper bound on the exit checks of one `vb` run: one per poll for the longest claim.
const POLLS_MAX: u64 =
    (BOARD_CALL_SECONDS_MAX as u64 + WAIT_SECONDS_MAX as u64) * 1000 / POLL_MILLIS + 1;
/// Upper bound on the arguments of one `vb` run.
const ARGS_MAX: usize = 24;
/// How long a claim holds its task before the board may give it to someone else.
const CLAIM_LEASE: &str = "1h";
/// Failure reasons that mean a claim found nothing, not that it went wrong.
const CLAIM_EMPTY_REASONS: [&str; 2] = ["timeout", "nothing_ready"];
/// The first word of every command `vb` is run with.
const VERBS: [&str; 5] = ["read", "tasks", "label", "reply", "post"];
/// The number of global arguments: `--json --url <url> --as <profile>`.
const GLOBAL_ARGS_LEN: usize = 5;
/// The longest decimal ID that fits a `u64`.
const ID_DIGITS_MAX: usize = 20;
/// Upper bound on the tasks one task waits for.
pub const NEEDS_MAX: usize = 64;
/// The open tasks one page of a listing asks `vb` for; `vb` takes at most 500.
pub const TASKS_PAGE_MAX: usize = 200;
/// Upper bound on the pages one listing reads; a subtree with more is an error.
pub const LISTING_PAGES_MAX: usize = 16;
/// Upper bound on the replies one thread read takes.
pub const REPLIES_MAX: usize = 10_000;
/// The schema a reviewer posts each finding under.
const FINDING_SCHEMA: &str = "rider-finding";
/// The reasons a fenced write gives when its claim is over or was never held.
const CLAIM_LOST_REASONS: [&str; 2] = ["claim_lost", "claim_not_held"];
/// The reasons `vb` gives when it could not reach the board at all.
const UNREACHABLE_REASONS: [&str; 1] = ["network_error"];
/// The length of a time as the board prints it: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
const TIMESTAMP_LEN: usize = 24;

/// A moment the board printed, in milliseconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub u64);

/// Who holds a task's claim, and until when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The board's actor, such as `local:user:zeke+opus-1`.
    pub holder: String,
    pub until: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    pub id: TaskId,
    pub text: String,
    pub state: TaskState,
    /// At most `LABELS_MAX` entries.
    pub labels: Vec<(String, String)>,
    /// The tasks this one waits for; at most `NEEDS_MAX`.
    pub needs: Vec<TaskId>,
    /// How many times the task has been claimed.
    pub epoch: u64,
    /// Present exactly when the task is claimed.
    pub claim: Option<Claim>,
    /// The cursor of the newest event that changed the task's message.
    pub version: Cursor,
}

/// A task as a listing shows it, with the channel it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedTask {
    pub view: TaskView,
    pub channel: String,
}

/// One page of a listing and the cursor the board printed with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<ListedTask>,
    pub cursor: Cursor,
}

/// A subtree's listed tasks: the first `TASKS_MAX` in the board's order, and a cursor from
/// before the first page, so a feed started there replays any change made while paging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub tasks: Vec<ListedTask>,
    pub cursor: Cursor,
    /// The listed tasks past `TASKS_MAX`, which the listing does not return.
    pub left_out: usize,
}

#[derive(Debug)]
pub enum BoardError {
    Spawn(String),
    /// `code` is `None` when a signal ended `vb`.
    Exit {
        code: Option<i32>,
        stderr: String,
    },
    Parse(String),
    TooManyLabels,
    /// A task waits for more than `NEEDS_MAX` tasks.
    TooManyNeeds,
    /// A subtree's listed tasks fill more than `LISTING_PAGES_MAX` pages.
    TooManyTasks,
    /// The board refused a write on task `id` because this profile's claim on it is over.
    ClaimLost {
        id: TaskId,
    },
    /// `vb` printed more than `OUTPUT_BYTES_MAX` bytes on one stream.
    OutputTooLarge,
    /// Reading from or writing to `vb` failed.
    Io(String),
    /// A message of `chars` characters, over `MESSAGE_CHARS_MAX`, was refused before `vb` ran.
    MessageTooLong {
        chars: usize,
    },
    /// `vb` ran past its time limit and was killed; `call` is its verb, such as `tasks claim`.
    TimedOut {
        call: &'static str,
        seconds: u32,
    },
    /// Signalling `vb`'s process group failed, so its descendants may still be running.
    Kill(String),
    /// `vb` could not reach the board; `stderr` is what it printed.
    Unreachable {
        stderr: String,
    },
}

impl fmt::Display for BoardError {
    /// One line saying how `vb` failed, with its own text cut short.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoardError::Spawn(message) => {
                write!(f, "vb could not be started: {}", one_line(message))
            }
            BoardError::Exit {
                code: Some(code),
                stderr,
            } => write!(f, "vb exited with code {code}: {}", one_line(stderr)),
            BoardError::Exit { code: None, stderr } => {
                write!(f, "vb was ended by a signal: {}", one_line(stderr))
            }
            BoardError::Parse(message) => {
                write!(
                    f,
                    "vb printed output rider cannot use: {}",
                    one_line(message)
                )
            }
            BoardError::TooManyLabels => write!(f, "the task has more than {LABELS_MAX} labels"),
            BoardError::TooManyNeeds => write!(f, "the task waits for more than {NEEDS_MAX} tasks"),
            BoardError::TooManyTasks => {
                let tasks = TASKS_PAGE_MAX * LISTING_PAGES_MAX;
                write!(f, "more than {tasks} tasks in one listing")
            }
            BoardError::ClaimLost { id } => write!(f, "the claim on task {} was lost", id.0),
            BoardError::OutputTooLarge => {
                write!(
                    f,
                    "vb printed more than {OUTPUT_BYTES_MAX} bytes on one stream"
                )
            }
            BoardError::Io(message) => {
                write!(
                    f,
                    "reading from or writing to vb failed: {}",
                    one_line(message)
                )
            }
            BoardError::MessageTooLong { chars } => write!(
                f,
                "a message of {chars} characters is over the board's {MESSAGE_CHARS_MAX}"
            ),
            BoardError::TimedOut { call, seconds } => {
                write!(
                    f,
                    "vb {call} did not finish within {seconds}s and was killed"
                )
            }
            BoardError::Kill(message) => {
                write!(
                    f,
                    "vb's process group could not be killed: {}",
                    one_line(message)
                )
            }
            BoardError::Unreachable { stderr } => {
                write!(f, "vb could not reach the board: {}", one_line(stderr))
            }
        }
    }
}

pub trait Board {
    fn read_task(&self, id: TaskId) -> Result<TaskView, BoardError>;
    /// None when nothing became ready within `wait_seconds`.
    fn claim_ready(
        &self,
        channel: &str,
        role_label: &str,
        wait_seconds: u32,
    ) -> Result<Option<TaskId>, BoardError>;
    fn set_label(&self, id: TaskId, key: &str, value: &str) -> Result<(), BoardError>;
    fn reply(&self, id: TaskId, text: &str) -> Result<(), BoardError>;
    fn post_structured(&self, id: TaskId, schema: &str, json: &str) -> Result<(), BoardError>;
    fn complete(&self, id: TaskId, result: &str) -> Result<(), BoardError>;
    fn fail(&self, id: TaskId, reason: &str) -> Result<(), BoardError>;
    fn block(&self, id: TaskId, reason: &str, kind: &str) -> Result<(), BoardError>;
    /// Claims task `id` for a lease, and returns it as claimed.
    fn claim_task(&self, id: TaskId) -> Result<TaskView, BoardError>;
    /// Extends the lease on task `id` from now. A claim that is over is `ClaimLost`.
    fn renew(&self, id: TaskId) -> Result<TaskView, BoardError>;
    /// Hands task `id` back, so it is ready again. A claim that is over is `ClaimLost`.
    fn release(&self, id: TaskId) -> Result<(), BoardError>;
    /// The open tasks in `subtree`, a channel prefix ending in `/`, and every channel below it.
    fn list_open(&self, subtree: &str) -> Result<Listing, BoardError>;
    /// The tasks in any state in `channel`, or below it when it ends in `/`, whose label `key`
    /// has one of `values`, none empty or holding `|`.
    fn list_labelled(
        &self,
        channel: &str,
        key: &str,
        values: &[&str],
    ) -> Result<Listing, BoardError>;
    /// The `rider-finding` replies in task `id`'s thread whose severity is `blocking`.
    fn blocking_findings(&self, id: TaskId) -> Result<usize, BoardError>;
}

/// Reaches the board by running `vb`, one process per call.
pub struct VbCli {
    pub vb: PathBuf,
    pub url: String,
    pub profile: String,
}

/// One `vb` command, before it becomes an argument list.
#[derive(Debug, Clone, Copy)]
enum Command<'a> {
    Read {
        id: TaskId,
    },
    Claim {
        channel: &'a str,
        role_label: &'a str,
        wait_seconds: u32,
    },
    ClaimId {
        id: TaskId,
    },
    Renew {
        id: TaskId,
    },
    Release {
        id: TaskId,
    },
    Cursor {
        subtree: &'a str,
    },
    List {
        subtree: &'a str,
        state: &'static str,
        label: Option<&'a str>,
        after: Option<Cursor>,
    },
    Label {
        id: TaskId,
        key: &'a str,
        value: &'a str,
    },
    Reply {
        id: TaskId,
        text: &'a str,
    },
    Post {
        id: TaskId,
        schema: &'a str,
        json: &'a str,
    },
    Complete {
        id: TaskId,
        result: &'a str,
    },
    Fail {
        id: TaskId,
        reason: &'a str,
    },
    Block {
        id: TaskId,
        reason: &'a str,
        kind: &'a str,
    },
}

/// What a finished `vb` run printed, and how it exited. `code` is `None` after a signal.
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `text` on one line: each run of whitespace becomes one space, and the line is cut to at
/// most `RESULT_CHARS_MAX` characters, on a character boundary.
pub(crate) fn one_line(text: &str) -> String {
    let mut line = String::with_capacity(RESULT_CHARS_MAX);
    let mut chars: usize = 0;
    // Every word holds at least one character, so no more words than this can fit.
    for word in text.split_whitespace().take(RESULT_CHARS_MAX) {
        if chars > 0 {
            line.push(' ');
            chars += 1;
        }
        for c in word.chars().take(RESULT_CHARS_MAX - chars) {
            line.push(c);
            chars += 1;
        }
        if chars >= RESULT_CHARS_MAX {
            break;
        }
    }
    let line = line.trim_end().to_string();
    assert!(line.chars().count() <= RESULT_CHARS_MAX);
    assert!(!line.contains(|c: char| c.is_whitespace() && c != ' '));
    line
}

/// The end of `text` in at most `chars_max` characters. When `text` is longer, the cut falls
/// after the window's first newline if it holds one, so the tail starts on a whole line.
pub fn message_tail(text: &str, chars_max: usize) -> &str {
    // `before` is the character just before the window; a newline there means the window
    // already starts on a whole line.
    let Some((cut, before)) = text.char_indices().rev().nth(chars_max) else {
        return text;
    };
    let window = &text[cut + before.len_utf8()..];
    let tail = match window.find('\n') {
        Some(newline) if before != '\n' && newline + 1 < window.len() => &window[newline + 1..],
        _ => window,
    };
    assert!(tail.chars().count() <= chars_max);
    assert!(text.ends_with(tail));
    tail
}

/// Refuses `text` the board would reject for its length, without running `vb`.
fn check_message(text: &str) -> Result<(), BoardError> {
    let chars = text.chars().count();
    if chars > MESSAGE_CHARS_MAX {
        return Err(BoardError::MessageTooLong { chars });
    }
    assert!(chars <= text.len());
    Ok(())
}

/// The words before the global options, and the free text that follows a final `--`.
fn command_words(command: &Command) -> (Vec<String>, Option<String>) {
    let to_strings =
        |words: &[&str]| -> Vec<String> { words.iter().map(|w| w.to_string()).collect() };
    let (words, text) = match *command {
        Command::Read { id } => (to_strings(&["read", &id.0.to_string()]), None),
        Command::Claim {
            channel,
            role_label,
            wait_seconds,
        } => {
            let label = format!("role={role_label}");
            let mut words = to_strings(&["tasks", "claim", "--ready", channel, "--label", &label]);
            words.extend(to_strings(&["--lease", CLAIM_LEASE]));
            if wait_seconds > 0 {
                words.extend(to_strings(&["--wait", &format!("{wait_seconds}s")]));
            }
            (words, None)
        }
        Command::ClaimId { id } => {
            let id = id.0.to_string();
            (
                to_strings(&["tasks", "claim", &id, "--lease", CLAIM_LEASE]),
                None,
            )
        }
        Command::Renew { id } => {
            let id = id.0.to_string();
            (
                to_strings(&["tasks", "renew", &id, "--lease", CLAIM_LEASE]),
                None,
            )
        }
        Command::Release { id } => (to_strings(&["tasks", "release", &id.0.to_string()]), None),
        Command::Cursor { subtree } => (to_strings(&["read", subtree, "--limit", "1"]), None),
        Command::List {
            subtree,
            state,
            label,
            after,
        } => {
            let limit = TASKS_PAGE_MAX.to_string();
            let mut words = to_strings(&["tasks", subtree, "--state", state, "--limit", &limit]);
            if let Some(label) = label {
                words.extend(to_strings(&["--label", label]));
            }
            if let Some(after) = after {
                words.extend(to_strings(&["--after", &after.0.to_string()]));
            }
            (words, None)
        }
        Command::Label { id, key, value } => {
            let pair = format!("{key}={value}");
            (to_strings(&["label", &id.0.to_string(), &pair]), None)
        }
        // A reply's text goes on stdin; `-` tells `vb` to read it there.
        Command::Reply { id, .. } => (to_strings(&["reply", &id.0.to_string(), "-"]), None),
        Command::Post { id, schema, json } => {
            let words = to_strings(&["post", &id.0.to_string(), "--schema", schema]);
            (words, Some(json.to_string()))
        }
        Command::Complete { id, result } => {
            let words = to_strings(&["tasks", "complete", &id.0.to_string()]);
            (words, Some(one_line(result)))
        }
        Command::Fail { id, reason } => (
            to_strings(&["tasks", "fail", &id.0.to_string()]),
            Some(one_line(reason)),
        ),
        Command::Block { id, reason, kind } => {
            let words = to_strings(&["tasks", "block", &id.0.to_string(), "--kind", kind]);
            (words, Some(one_line(reason)))
        }
    };
    assert!(
        VERBS.contains(&words[0].as_str()),
        "every command starts with a known verb"
    );
    assert!(words.len() + GLOBAL_ARGS_LEN + 2 <= ARGS_MAX);
    (words, text)
}

/// The argument list for `command`: its words, then the global `--json --url <url> --as <profile>`, then `--` and any free text, so
/// text that starts with `-` is never read as an option.
fn command_args(command: &Command, url: &str, profile: &str) -> Vec<String> {
    assert!(!url.is_empty(), "the board URL is never empty");
    assert!(!profile.is_empty(), "the profile is never empty");
    let (mut args, text) = command_words(command);
    let words_len = args.len();
    let global = ["--json", "--url", url, "--as", profile];
    assert_eq!(global.len(), GLOBAL_ARGS_LEN);
    args.extend(global.map(String::from));
    if let Some(text) = text {
        args.extend(["--".to_string(), text]);
    }
    assert!(args.len() <= ARGS_MAX);
    assert_eq!(
        args[words_len], "--json",
        "global options follow the command's words"
    );
    args
}

/// The text `command` sends on stdin, if any. Only a reply does: it may span many lines.
fn command_stdin<'a>(command: &Command<'a>) -> Option<&'a str> {
    let stdin = match *command {
        Command::Reply { text, .. } => Some(text),
        _ => None,
    };
    assert!(stdin.is_none() || matches!(command, Command::Reply { .. }));
    stdin
}

/// The name `command` goes by in a timeout, and how many seconds its run may take.
fn command_limit(command: &Command) -> (&'static str, u32) {
    let (call, wait_seconds) = match *command {
        Command::Read { .. } => ("read", 0),
        Command::Claim { wait_seconds, .. } => ("tasks claim", wait_seconds),
        Command::ClaimId { .. } => ("tasks claim", 0),
        Command::Renew { .. } => ("tasks renew", 0),
        Command::Release { .. } => ("tasks release", 0),
        Command::Cursor { .. } => ("read", 0),
        Command::List { .. } => ("tasks", 0),
        Command::Label { .. } => ("label", 0),
        Command::Reply { .. } => ("reply", 0),
        Command::Post { .. } => ("post", 0),
        Command::Complete { .. } => ("tasks complete", 0),
        Command::Fail { .. } => ("tasks fail", 0),
        Command::Block { .. } => ("tasks block", 0),
    };
    assert!(
        wait_seconds <= WAIT_SECONDS_MAX,
        "a claim waits at most an hour"
    );
    let seconds = CALL_SECONDS + wait_seconds;
    assert!(seconds >= CALL_SECONDS && CALL_SECONDS <= BOARD_CALL_SECONDS_MAX);
    (call, seconds)
}

/// The outcome a successful run of `command` reports, and the task it acts on, for writes.
fn expected_write(command: &Command) -> Option<(&'static str, TaskId)> {
    match *command {
        Command::Read { .. }
        | Command::Claim { .. }
        | Command::ClaimId { .. }
        | Command::Renew { .. }
        | Command::Cursor { .. }
        | Command::List { .. } => None,
        Command::Release { id } => Some(("released", id)),
        Command::Label { id, .. } => Some(("labeled", id)),
        Command::Reply { id, .. } | Command::Post { id, .. } => Some(("posted", id)),
        Command::Complete { id, .. } => Some(("completed", id)),
        Command::Fail { id, .. } => Some(("failed", id)),
        Command::Block { id, .. } => Some(("blocked", id)),
    }
}

/// Deserializes one of the board's JSON shapes from bounded output.
fn parse<T: DeserializeOwned>(json: &str) -> Result<T, BoardError> {
    assert!(
        json.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    serde_json::from_str(json).map_err(|error| BoardError::Parse(format!("{error}")))
}

/// Deserializes a field that must be present but may be null.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// A task's state as the board prints it.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WireState {
    Waiting,
    Ready,
    Claimed,
    Blocked,
    Completed,
    Failed,
    Canceled,
}

impl From<WireState> for TaskState {
    fn from(state: WireState) -> TaskState {
        match state {
            WireState::Waiting => TaskState::Waiting,
            WireState::Ready => TaskState::Ready,
            WireState::Claimed => TaskState::Claimed,
            WireState::Blocked => TaskState::Blocked,
            WireState::Completed => TaskState::Completed,
            WireState::Failed => TaskState::Failed,
            WireState::Canceled => TaskState::Canceled,
        }
    }
}

/// `task.claim` of a claimed task.
#[derive(Deserialize)]
struct WireHolder {
    actor: String,
    until: String,
}

/// The `task` object of a task's message.
#[derive(Deserialize)]
struct WireTask {
    state: WireState,
    epoch: u64,
    needs: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    claim: Option<WireHolder>,
}

/// A task's message, as `read` prints under `message` and `claim` under `task`.
#[derive(Deserialize)]
struct WireMessage {
    id: String,
    text: String,
    labels: BTreeMap<String, String>,
    task: WireTask,
    version: u64,
}

/// `vb read <id> --json`: `{message, replies, cursor, schedules}`.
#[derive(Deserialize)]
struct WireRead {
    message: WireMessage,
}

/// The replies of `vb read <id> --json`, each with its structured blocks.
#[derive(Deserialize)]
struct WireThread {
    replies: Vec<WireReply>,
}

#[derive(Deserialize)]
struct WireReply {
    blocks: Vec<WireBlock>,
}

/// A structured block: `{schema, version, data}`.
#[derive(Deserialize)]
struct WireBlock {
    schema: String,
    data: serde_json::Value,
}

/// A successful `vb tasks claim --json`: `{task, claim}`, the task null when none was ready.
#[derive(Deserialize)]
struct WireClaimed {
    #[serde(deserialize_with = "nullable")]
    task: Option<WireMessage>,
}

/// The `claim` of a lease.
#[derive(Deserialize)]
struct WireLeaseClaim {
    epoch: u64,
    until: String,
}

/// `vb tasks claim <id>` or `vb tasks renew`: `{task, claim: {epoch, until}}`.
#[derive(Deserialize)]
struct WireLease {
    task: WireMessage,
    claim: WireLeaseClaim,
}

#[derive(Deserialize)]
struct WireListed {
    message: WireListedMessage,
}

/// A listed task's message: a task's message and the channel it is in.
#[derive(Deserialize)]
struct WireListedMessage {
    channel: String,
    #[serde(flatten)]
    message: WireMessage,
}

/// One page of `vb tasks <subtree> --json`: `{tasks: [{message, ...}], cursor}`.
#[derive(Deserialize)]
struct WireListing {
    tasks: Vec<WireListed>,
    cursor: String,
}

/// Any read that prints the board's `cursor`, a decimal string.
#[derive(Deserialize)]
struct WireCursor {
    cursor: String,
}

/// A failed run's JSON: `{status: "error", reason, message}`.
#[derive(Deserialize)]
struct WireFailure {
    status: Option<String>,
    reason: String,
}

#[derive(Deserialize)]
struct WireTaskRef {
    id: String,
}

#[derive(Deserialize)]
struct WireMessageRef {
    id: String,
    re: Option<String>,
}

/// A write's output: `{task, outcome}` or `{message, outcome}`.
#[derive(Deserialize)]
struct WireWrite {
    outcome: String,
    task: Option<WireTaskRef>,
    message: Option<WireMessageRef>,
}

/// An ID as the board prints it: canonical decimal, no sign and no leading zeros.
fn parse_id_text(id: &str) -> Result<TaskId, BoardError> {
    let canonical = !id.is_empty()
        && id.len() <= ID_DIGITS_MAX
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && (id == "0" || !id.starts_with('0'));
    if !canonical {
        return Err(BoardError::Parse(format!(
            "id {id:?} is not canonical decimal"
        )));
    }
    let number = id.parse::<u64>();
    let number = number.map_err(|_| BoardError::Parse(format!("id {id:?} is too big")))?;
    assert_eq!(
        number.to_string(),
        id,
        "a canonical id prints back as itself"
    );
    Ok(TaskId(number))
}

/// A time as the board prints it, `YYYY-MM-DDTHH:MM:SS.mmmZ`, from 1970 on.
pub fn parse_timestamp(text: &str) -> Result<Timestamp, BoardError> {
    let bad = || BoardError::Parse(format!("time {text:?} is not YYYY-MM-DDTHH:MM:SS.mmmZ"));
    let bytes = text.as_bytes();
    // jiff takes any RFC 3339 time, and a leap second as :59; the board prints only this
    // one shape of it, and never a leap second.
    let shaped = bytes.len() == TIMESTAMP_LEN
        && text.is_ascii()
        && [(10, b'T'), (19, b'.'), (23, b'Z')]
            .iter()
            .all(|&(at, byte)| bytes[at] == byte)
        && &text[17..19] != "60";
    if !shaped {
        return Err(bad());
    }
    let parsed: jiff::Timestamp = text.parse().map_err(|_| bad())?;
    let millis = u64::try_from(parsed.as_millisecond()).map_err(|_| bad())?;
    let stamp = Timestamp(millis);
    assert!(stamp.0 / 1000 == u64::try_from(parsed.as_second()).unwrap_or(u64::MAX));
    Ok(stamp)
}

fn holder_from_wire(holder: WireHolder) -> Result<Claim, BoardError> {
    if holder.actor.is_empty() {
        return Err(BoardError::Parse("an empty task.claim.actor".into()));
    }
    let claim = Claim {
        until: parse_timestamp(&holder.until)?,
        holder: holder.actor,
    };
    assert!(!claim.holder.is_empty());
    Ok(claim)
}

/// The task a message describes.
fn view_from_wire(message: WireMessage) -> Result<TaskView, BoardError> {
    let WireMessage {
        id,
        text,
        labels,
        task,
        version,
    } = message;
    if labels.len() > LABELS_MAX {
        return Err(BoardError::TooManyLabels);
    }
    if task.needs.len() > NEEDS_MAX {
        return Err(BoardError::TooManyNeeds);
    }
    let state = TaskState::from(task.state);
    let claim = task.claim.map(holder_from_wire).transpose()?;
    if claim.is_some() != (state == TaskState::Claimed) {
        return Err(BoardError::Parse(format!(
            "a {state:?} task {} a claim",
            if claim.is_some() { "with" } else { "without" }
        )));
    }
    let needs: Result<Vec<TaskId>, BoardError> =
        task.needs.iter().map(|need| parse_id_text(need)).collect();
    let view = TaskView {
        id: parse_id_text(&id)?,
        text,
        state,
        labels: labels.into_iter().collect(),
        needs: needs?,
        epoch: task.epoch,
        claim,
        version: Cursor(version),
    };
    assert!(view.text.len() < OUTPUT_BYTES_MAX);
    assert_eq!(view.claim.is_some(), view.state == TaskState::Claimed);
    Ok(view)
}

/// A task from the output of `vb read <id> --json`.
pub fn parse_task_view(json: &str) -> Result<TaskView, BoardError> {
    let read: WireRead = parse(json)?;
    let view = view_from_wire(read.message)?;
    assert!(view.labels.len() <= LABELS_MAX);
    Ok(view)
}

/// How many `rider-finding` blocks in the replies of `vb read <id> --json` are `blocking`.
pub fn parse_blocking_findings(json: &str) -> Result<usize, BoardError> {
    let thread: WireThread = parse(json)?;
    if thread.replies.len() > REPLIES_MAX {
        return Err(BoardError::Parse(format!(
            "a thread of {} replies, over {REPLIES_MAX}",
            thread.replies.len()
        )));
    }
    let blocks = thread.replies.iter().flat_map(|reply| &reply.blocks);
    let findings = blocks.filter(|block| block.schema == FINDING_SCHEMA);
    let mut blocking: usize = 0;
    for finding in findings {
        match finding.data["severity"].as_str() {
            Some("blocking") => blocking += 1,
            Some("non-blocking") => {}
            _ => return Err(BoardError::Parse("a finding without its severity".into())),
        }
    }
    assert!(blocking <= OUTPUT_BYTES_MAX);
    Ok(blocking)
}

/// The claimed task from the output of a successful `vb tasks claim --json`.
/// `None` when the board printed a null task.
pub fn parse_claim(json: &str) -> Result<Option<TaskId>, BoardError> {
    let claimed: WireClaimed = parse(json)?;
    let Some(task) = claimed.task else {
        return Ok(None);
    };
    let view = view_from_wire(task)?;
    if view.state != TaskState::Claimed {
        return Err(BoardError::Parse(format!(
            "claimed task is {:?}",
            view.state
        )));
    }
    Ok(Some(view.id))
}

/// The task held under a lease, from the output of `vb tasks claim <id>` or `vb tasks renew`,
/// where the lease must match the task's own claim.
pub fn parse_lease(json: &str) -> Result<TaskView, BoardError> {
    let lease: WireLease = parse(json)?;
    let view = view_from_wire(lease.task)?;
    let epoch = lease.claim.epoch;
    let until = parse_timestamp(&lease.claim.until)?;
    let held_until = view.claim.as_ref().map(|claim| claim.until);
    if epoch != view.epoch || held_until != Some(until) {
        return Err(BoardError::Parse(format!(
            "the lease (epoch {epoch}) does not match task {} (epoch {})",
            view.id.0, view.epoch
        )));
    }
    assert_eq!(view.state, TaskState::Claimed);
    Ok(view)
}

/// One page of `vb tasks <subtree> --json`.
pub fn parse_task_list(json: &str) -> Result<TaskPage, BoardError> {
    let listing: WireListing = parse(json)?;
    let count = listing.tasks.len();
    if count > TASKS_PAGE_MAX {
        return Err(BoardError::Parse(format!(
            "a page of {count} tasks, over the {TASKS_PAGE_MAX} asked for"
        )));
    }
    let cursor = Cursor(parse_id_text(&listing.cursor)?.0);
    let tasks: Result<Vec<ListedTask>, BoardError> = (listing.tasks.into_iter())
        .map(|listed| {
            let WireListedMessage { channel, message } = listed.message;
            if channel.is_empty() {
                return Err(BoardError::Parse(
                    "a listed task in an empty channel".into(),
                ));
            }
            let view = view_from_wire(message)?;
            Ok(ListedTask { view, channel })
        })
        .collect();
    let tasks = tasks?;
    assert_eq!(tasks.len(), count, "no task was dropped");
    Ok(TaskPage { tasks, cursor })
}

/// The `reason` of a failed run, from the JSON it printed.
pub fn parse_failure_reason(stderr: &str) -> Option<String> {
    assert!(
        stderr.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    let failure: WireFailure = parse(stderr).ok()?;
    if failure.status.as_deref() == Some("ok") {
        return None;
    }
    Some(failure.reason)
}

/// The `outcome` of a write and the task it acted on. Task writes print `{task, outcome}`;
/// `label` prints `{message: {id}, outcome}`; `reply` and `post` print the new reply as
/// `{message, outcome}`, whose `re` is the task.
fn parse_write(json: &str) -> Result<(String, TaskId), BoardError> {
    let write: WireWrite = parse(json)?;
    if write.outcome.is_empty() {
        return Err(BoardError::Parse("an empty outcome".into()));
    }
    let id = match (write.task, write.message) {
        (Some(task), _) => parse_id_text(&task.id)?,
        (None, Some(message)) => parse_id_text(&message.re.unwrap_or(message.id))?,
        (None, None) => return Err(BoardError::Parse("no task or message".into())),
    };
    assert!(!write.outcome.is_empty());
    Ok((write.outcome, id))
}

/// What a claim's run means: a task, nothing ready in time, or a failure.
fn claim_outcome(output: &Output) -> Result<Option<TaskId>, BoardError> {
    assert!(output.stdout.len() <= OUTPUT_BYTES_MAX);
    assert!(output.stderr.len() <= OUTPUT_BYTES_MAX);
    if output.code == Some(0) {
        return parse_claim(&output.stdout);
    }
    // A signal is never "nothing ready", whatever was printed before it.
    if output.code.is_some() {
        // The board prints a failure's JSON on stderr; stdout is read too, in case.
        let reason =
            parse_failure_reason(&output.stderr).or_else(|| parse_failure_reason(&output.stdout));
        if reason.is_some_and(|reason| CLAIM_EMPTY_REASONS.contains(&reason.as_str())) {
            return Ok(None);
        }
    }
    Err(exit_error(output))
}

/// The error for a failed run on task `id`: `ClaimLost` when the board refused it for the
/// claim, otherwise how `vb` exited.
fn failed_on(id: TaskId, output: &Output) -> BoardError {
    assert!(output.code != Some(0), "only a failed run has an error");
    // A signal is never the board's answer, whatever was printed before it.
    if output.code.is_some() {
        let reason =
            parse_failure_reason(&output.stderr).or_else(|| parse_failure_reason(&output.stdout));
        if reason.is_some_and(|reason| CLAIM_LOST_REASONS.contains(&reason.as_str())) {
            return BoardError::ClaimLost { id };
        }
    }
    exit_error(output)
}

/// The error for a failed run: `Unreachable` when `vb` could not reach the board, otherwise
/// how `vb` exited.
fn exit_error(output: &Output) -> BoardError {
    assert!(output.code != Some(0), "only a failed run has an error");
    if output.code.is_some() {
        let reason =
            parse_failure_reason(&output.stderr).or_else(|| parse_failure_reason(&output.stdout));
        if reason.is_some_and(|reason| UNREACHABLE_REASONS.contains(&reason.as_str())) {
            return BoardError::Unreachable {
                stderr: output.stderr.clone(),
            };
        }
    }
    BoardError::Exit {
        code: output.code,
        stderr: output.stderr.clone(),
    }
}

/// The task a lease's run holds, from `vb tasks claim <id>` or `vb tasks renew`.
fn lease_outcome(id: TaskId, output: &Output) -> Result<TaskView, BoardError> {
    if output.code != Some(0) {
        return Err(failed_on(id, output));
    }
    let view = parse_lease(&output.stdout)?;
    if view.id != id {
        return Err(BoardError::Parse(format!(
            "asked for {id:?}, held {:?}",
            view.id
        )));
    }
    assert_eq!(view.state, TaskState::Claimed);
    Ok(view)
}

/// Whether a write's run succeeded with the outcome `command` expects, on its task.
fn write_outcome(command: &Command, output: &Output) -> Result<(), BoardError> {
    let expected = expected_write(command);
    let (expected_outcome, expected_id) =
        expected.ok_or_else(|| BoardError::Parse("not a write".into()))?;
    assert!(!expected_outcome.is_empty());
    if output.code != Some(0) {
        return Err(failed_on(expected_id, output));
    }
    let (outcome, id) = parse_write(&output.stdout)?;
    if outcome != expected_outcome || id != expected_id {
        return Err(BoardError::Parse(format!(
            "wrote {outcome:?} on {id:?}, not {expected_outcome:?} on {expected_id:?}"
        )));
    }
    Ok(())
}

/// Reads at most `OUTPUT_BYTES_MAX` bytes; more is an error.
fn read_bounded(stream: &mut impl Read, name: &str) -> Result<String, BoardError> {
    let limit = u64::try_from(OUTPUT_BYTES_MAX + 1).unwrap_or(u64::MAX);
    let mut bytes: Vec<u8> = Vec::new();
    let read = stream.take(limit).read_to_end(&mut bytes);
    read.map_err(|error| BoardError::Io(format!("reading {name}: {error}")))?;
    if bytes.len() > OUTPUT_BYTES_MAX {
        return Err(BoardError::OutputTooLarge);
    }
    let text =
        String::from_utf8(bytes).map_err(|_| BoardError::Parse(format!("{name} is not UTF-8")))?;
    assert!(text.len() <= OUTPUT_BYTES_MAX);
    Ok(text)
}

type Worker<T> = JoinHandle<Result<T, BoardError>>;

fn start_worker<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> Result<T, BoardError> + Send + 'static,
) -> Result<Worker<T>, BoardError> {
    assert!(!name.is_empty());
    let builder = thread::Builder::new().name(format!("vb-{name}"));
    let handle = builder.spawn(work);
    handle.map_err(|error| BoardError::Spawn(format!("starting the {name} thread: {error}")))
}

fn finish_worker<T>(worker: Worker<T>, name: &str) -> Result<T, BoardError> {
    assert!(!name.is_empty());
    let joined = worker.join();
    joined.map_err(|_| BoardError::Io(format!("the {name} thread panicked")))?
}

/// Writes `text` to `stdin` and closes it.
fn feed(mut stdin: ChildStdin, text: String) -> Result<(), BoardError> {
    assert!(text.len() <= OUTPUT_BYTES_MAX);
    let written = stdin.write_all(text.as_bytes());
    written.map_err(|error| BoardError::Io(format!("writing stdin: {error}")))?;
    drop(stdin);
    Ok(())
}

/// Polls until `child` has exited and `done` holds, or until `deadline`. `None` means time ran
/// out first.
fn wait_until(
    child: &mut dyn ChildWrapper,
    done: impl Fn() -> bool,
    deadline: Instant,
) -> Result<Option<ExitStatus>, BoardError> {
    let mut status = None;
    for _ in 0..POLLS_MAX {
        if status.is_none() {
            let polled = child.try_wait();
            status = polled.map_err(|error| BoardError::Io(format!("waiting: {error}")))?;
        }
        if status.is_some() && done() {
            return Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(deadline.saturating_duration_since(now).min(POLL_INTERVAL));
    }
    Ok(None)
}

/// Sends SIGKILL to the process group `child` leads, then kills and reaps `child`. A group that
/// is gone afterwards is not an error. A child that could not be killed is never waited on.
pub(crate) fn kill_group(child: &mut dyn ChildWrapper) -> Result<(), BoardError> {
    let id = child.id();
    assert!(id > 1, "never signal the caller's own group");
    let group = i32::try_from(id).ok().and_then(Pid::from_raw);
    let group = group.ok_or_else(|| BoardError::Kill(format!("group {id} is not a pid")))?;
    let signalled = child.start_kill();
    if signalled.is_err() {
        // The group refused the signal, so the leader is killed on its own before any wait.
        if let Err(error) = child.inner_mut().start_kill()
            && !matches!(child.try_wait(), Ok(Some(_)))
        {
            return Err(BoardError::Kill(format!("child {id}: {error}")));
        }
    }
    let _ = child.wait();
    // Darwin refuses (EPERM) to signal a group whose only member is the exited, unreaped
    // leader. Once the leader is reaped, only an empty group answers the probe with ESRCH.
    match signalled {
        Ok(()) => Ok(()),
        Err(_) if test_kill_process_group(group) == Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(BoardError::Kill(format!("group {id}: {error}"))),
    }
}

/// Feeds `input` to `child` and reads both of its streams, each on its own thread so no full
/// pipe can stall `vb`. Within `seconds`, `vb` must exit and every pipe must close; otherwise
/// the whole process group is killed.
fn collect(
    child: &mut dyn ChildWrapper,
    input: Option<String>,
    call: &'static str,
    seconds: u32,
) -> Result<Output, BoardError> {
    assert!(seconds >= CALL_SECONDS);
    let deadline = Instant::now() + Duration::from_secs(u64::from(seconds));
    let mut stdout = child
        .stdout()
        .take()
        .ok_or_else(|| BoardError::Spawn("no stdout".into()))?;
    let mut stderr = child
        .stderr()
        .take()
        .ok_or_else(|| BoardError::Spawn("no stderr".into()))?;
    let writer = match (input, child.stdin().take()) {
        (Some(text), Some(stdin)) => Some(start_worker("stdin", move || feed(stdin, text))?),
        (None, None) => None,
        _ => {
            return Err(BoardError::Spawn(
                "stdin is not set up for the input".into(),
            ));
        }
    };
    let errors = start_worker("stderr", move || read_bounded(&mut stderr, "stderr"))?;
    let reader = start_worker("stdout", move || read_bounded(&mut stdout, "stdout"))?;
    let done = || {
        reader.is_finished()
            && errors.is_finished()
            && writer.as_ref().is_none_or(JoinHandle::is_finished)
    };
    let Some(status) = wait_until(child, done, deadline)? else {
        kill_group(child)?;
        // Every writer in the group is dead, so the pipes close and the workers finish. One
        // that `vb` moved out of its group may still hold a pipe: its worker is abandoned.
        let drained = wait_until(child, done, Instant::now() + KILL_GRACE);
        if matches!(drained, Ok(Some(_))) {
            let _ = finish_worker(reader, "stdout");
            let _ = finish_worker(errors, "stderr");
            let _ = writer.map(|writer| finish_worker(writer, "stdin"));
        }
        return Err(BoardError::TimedOut { call, seconds });
    };
    let stdout = finish_worker(reader, "stdout");
    let stderr = finish_worker(errors, "stderr");
    let written = writer.map(|writer| finish_worker(writer, "stdin"));
    let output = Output {
        code: status.code(),
        stdout: stdout?,
        stderr: stderr?,
    };
    // A failed write matters only when `vb` claims success; otherwise its exit says more.
    if output.code == Some(0) {
        written.unwrap_or(Ok(()))?;
    }
    assert!(output.stdout.len() <= OUTPUT_BYTES_MAX);
    assert!(output.stderr.len() <= OUTPUT_BYTES_MAX);
    Ok(output)
}

impl VbCli {
    fn run(&self, command: &Command) -> Result<Output, BoardError> {
        assert!(
            !self.vb.as_os_str().is_empty(),
            "the vb path is never empty"
        );
        let args = command_args(command, &self.url, &self.profile);
        let input = command_stdin(command).map(str::to_string);
        let stdin = if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        };
        let mut process = Process::new(&self.vb);
        process
            .args(&args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let spawned = CommandWrap::from(process)
            .wrap(ProcessGroup::leader())
            .spawn();
        let mut child = spawned
            .map_err(|error| BoardError::Spawn(format!("{}: {error}", self.vb.display())))?;
        let (call, seconds) = command_limit(command);
        let output = collect(child.as_mut(), input, call, seconds);
        // A thread could not start or a stream broke: do not leave `vb` running. A timeout
        // or a failed kill has already tried to kill its group.
        match output {
            Err(BoardError::TimedOut { .. } | BoardError::Kill(_)) | Ok(_) => output,
            Err(error) => kill_group(child.as_mut()).and(Err(error)),
        }
    }

    fn write(&self, command: &Command) -> Result<(), BoardError> {
        assert!(
            expected_write(command).is_some(),
            "only writes report an outcome"
        );
        let output = self.run(command)?;
        write_outcome(command, &output)
    }

    /// The stdout of a read that exited 0.
    fn read(&self, command: &Command) -> Result<String, BoardError> {
        assert!(
            expected_write(command).is_none(),
            "a write reports an outcome"
        );
        let output = self.run(command)?;
        if output.code != Some(0) {
            return Err(exit_error(&output));
        }
        assert!(output.stdout.len() <= OUTPUT_BYTES_MAX);
        Ok(output.stdout)
    }
}

impl Board for VbCli {
    fn read_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
        assert!(!self.url.is_empty() && !self.profile.is_empty());
        let view = parse_task_view(&self.read(&Command::Read { id })?)?;
        if view.id != id {
            return Err(BoardError::Parse(format!(
                "asked for {id:?}, read {:?}",
                view.id
            )));
        }
        Ok(view)
    }

    fn claim_ready(
        &self,
        channel: &str,
        role_label: &str,
        wait_seconds: u32,
    ) -> Result<Option<TaskId>, BoardError> {
        assert!(!channel.is_empty() && !role_label.is_empty());
        assert!(
            wait_seconds <= WAIT_SECONDS_MAX,
            "a claim waits at most an hour"
        );
        let output = self.run(&Command::Claim {
            channel,
            role_label,
            wait_seconds,
        })?;
        claim_outcome(&output)
    }

    fn set_label(&self, id: TaskId, key: &str, value: &str) -> Result<(), BoardError> {
        assert!(!key.is_empty(), "a label key is never empty");
        assert!(!key.contains('='), "a label key never holds '='");
        self.write(&Command::Label { id, key, value })
    }

    fn reply(&self, id: TaskId, text: &str) -> Result<(), BoardError> {
        assert!(!text.is_empty(), "a reply always says something");
        assert!(text.len() <= OUTPUT_BYTES_MAX);
        check_message(text)?;
        self.write(&Command::Reply { id, text })
    }

    fn post_structured(&self, id: TaskId, schema: &str, json: &str) -> Result<(), BoardError> {
        assert!(schema.contains('@'), "a schema is named as name@version");
        assert!(!json.is_empty() && json.len() <= OUTPUT_BYTES_MAX);
        check_message(json)?;
        self.write(&Command::Post { id, schema, json })
    }

    fn complete(&self, id: TaskId, result: &str) -> Result<(), BoardError> {
        assert!(
            !result.trim().is_empty(),
            "a completion always states its result"
        );
        assert!(result.len() <= OUTPUT_BYTES_MAX);
        self.write(&Command::Complete { id, result })
    }

    fn fail(&self, id: TaskId, reason: &str) -> Result<(), BoardError> {
        assert!(
            !reason.trim().is_empty(),
            "a failure always states its reason"
        );
        assert!(reason.len() <= OUTPUT_BYTES_MAX);
        self.write(&Command::Fail { id, reason })
    }

    fn block(&self, id: TaskId, reason: &str, kind: &str) -> Result<(), BoardError> {
        assert!(
            !reason.trim().is_empty(),
            "a block always states its reason"
        );
        assert!(!kind.is_empty(), "a block always has a kind");
        self.write(&Command::Block { id, reason, kind })
    }

    fn claim_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
        assert!(!self.url.is_empty() && !self.profile.is_empty());
        let output = self.run(&Command::ClaimId { id })?;
        let view = lease_outcome(id, &output)?;
        assert_eq!(view.id, id);
        Ok(view)
    }

    fn renew(&self, id: TaskId) -> Result<TaskView, BoardError> {
        assert!(!self.url.is_empty() && !self.profile.is_empty());
        let output = self.run(&Command::Renew { id })?;
        let view = lease_outcome(id, &output)?;
        assert_eq!(view.id, id);
        Ok(view)
    }

    fn release(&self, id: TaskId) -> Result<(), BoardError> {
        assert!(!self.url.is_empty() && !self.profile.is_empty());
        self.write(&Command::Release { id })
    }

    fn list_open(&self, subtree: &str) -> Result<Listing, BoardError> {
        assert!(
            subtree.ends_with('/'),
            "a subtree is a channel prefix ending in /"
        );
        self.list(subtree, "open", None)
    }

    fn list_labelled(
        &self,
        channel: &str,
        key: &str,
        values: &[&str],
    ) -> Result<Listing, BoardError> {
        assert!(!key.is_empty() && !key.contains('=') && !key.starts_with('-'));
        assert!(!values.is_empty());
        assert!(
            (values.iter()).all(|value| !value.is_empty() && !value.contains('|')),
            "a label value is one alternative"
        );
        let label = format!("{key}={}", values.join("|"));
        self.list(channel, "all", Some(&label))
    }

    fn blocking_findings(&self, id: TaskId) -> Result<usize, BoardError> {
        let json = self.read(&Command::Read { id })?;
        let view = parse_task_view(&json)?;
        if view.id != id {
            return Err(BoardError::Parse(format!(
                "asked for {id:?}, read {:?}",
                view.id
            )));
        }
        parse_blocking_findings(&json)
    }
}

impl VbCli {
    fn list(
        &self,
        subtree: &str,
        state: &'static str,
        label: Option<&str>,
    ) -> Result<Listing, BoardError> {
        assert!(state == "open" || state == "all");
        assert!(
            !subtree.is_empty() && !subtree.starts_with('-'),
            "a channel is never read as an option"
        );
        let read: WireCursor = parse(&self.read(&Command::Cursor { subtree })?)?;
        let cursor = Cursor(parse_id_text(&read.cursor)?.0);
        let mut tasks: Vec<ListedTask> = Vec::new();
        let mut left_out: usize = 0;
        let mut after: Option<Cursor> = None;
        for _ in 0..LISTING_PAGES_MAX {
            let list = Command::List {
                subtree,
                state,
                label,
                after,
            };
            let page = parse_task_list(&self.read(&list)?)?;
            let full = page.tasks.len() == TASKS_PAGE_MAX;
            let first = page.tasks.first().map(|task| task.view.id.0);
            if first
                .zip(after)
                .is_some_and(|(first, after)| first <= after.0)
            {
                return Err(BoardError::Parse(format!(
                    "a page after {} starts at task {}",
                    after.map_or(0, |after| after.0),
                    first.unwrap_or(0)
                )));
            }
            // A full page's cursor is its last task's id, where the next page starts; any
            // other page's cursor is the board's event cursor as of the listing.
            let last = page.tasks.last().map(|task| task.view.id.0);
            if full && last != Some(page.cursor.0) {
                return Err(BoardError::Parse(format!(
                    "a full page's cursor {} is not its last task",
                    page.cursor.0
                )));
            }
            let room = TASKS_MAX.saturating_sub(tasks.len());
            let over = page.tasks.len().saturating_sub(room);
            left_out = left_out.checked_add(over).ok_or(BoardError::TooManyTasks)?;
            tasks.extend(page.tasks.into_iter().take(room));
            assert!(tasks.len() <= TASKS_MAX);
            if !full {
                return Ok(Listing {
                    tasks,
                    cursor,
                    left_out,
                });
            }
            after = Some(page.cursor);
        }
        Err(BoardError::TooManyTasks)
    }
}

/// A board held in memory, for tests in this crate.
#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::{
        Board, BoardError, ListedTask, Listing, TaskId, TaskState, TaskView, check_message,
    };
    use crate::feed::Cursor;

    /// Upper bound on the calls one fake board records.
    pub const CALLS_MAX: usize = 1024;

    /// Serves `tasks` to `read_task`, hands out `claims` in order, and records every other
    /// call as one line, such as `"set_label 7 branch=rider/7-x"`. A method named in `failing`
    /// records its call, if it records calls, and then returns an error.
    pub struct FakeBoard {
        pub tasks: RefCell<Vec<TaskView>>,
        pub claims: RefCell<VecDeque<TaskId>>,
        pub calls: RefCell<Vec<String>>,
        pub failing: RefCell<Vec<&'static str>>,
    }

    impl FakeBoard {
        pub fn new(tasks: Vec<TaskView>) -> FakeBoard {
            FakeBoard {
                tasks: RefCell::new(tasks),
                claims: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Vec::new()),
                failing: RefCell::new(Vec::new()),
            }
        }

        /// Moves task `id` to `state`, as a worker or the lead would.
        pub fn set_state(&self, id: TaskId, state: TaskState) {
            let mut tasks = self.tasks.borrow_mut();
            let task = tasks.iter_mut().find(|task| task.id == id);
            task.unwrap_or_else(|| panic!("no task {}", id.0)).state = state;
        }

        fn scripted(&self, method: &str) -> Result<(), BoardError> {
            if !self.failing.borrow().contains(&method) {
                return Ok(());
            }
            Err(BoardError::Exit {
                code: Some(1),
                stderr: format!("{method} failed"),
            })
        }

        fn record(&self, call: String) -> Result<(), BoardError> {
            let method = call.split(' ').next().unwrap_or_default().to_string();
            let mut calls = self.calls.borrow_mut();
            assert!(
                calls.len() < CALLS_MAX,
                "a test makes at most CALLS_MAX calls"
            );
            calls.push(call);
            drop(calls);
            self.scripted(&method)
        }
    }

    impl Board for FakeBoard {
        fn read_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
            self.scripted("read_task")?;
            let tasks = self.tasks.borrow();
            let task = tasks.iter().find(|task| task.id == id);
            task.cloned().ok_or_else(|| BoardError::Exit {
                code: Some(1),
                stderr: format!("no task {}", id.0),
            })
        }

        fn claim_ready(
            &self,
            channel: &str,
            role_label: &str,
            wait_seconds: u32,
        ) -> Result<Option<TaskId>, BoardError> {
            self.record(format!("claim_ready {channel} {role_label} {wait_seconds}"))?;
            Ok(self.claims.borrow_mut().pop_front())
        }

        fn set_label(&self, id: TaskId, key: &str, value: &str) -> Result<(), BoardError> {
            self.record(format!("set_label {} {key}={value}", id.0))?;
            let mut tasks = self.tasks.borrow_mut();
            if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                task.labels.retain(|(name, _)| name != key);
                task.labels.push((key.to_string(), value.to_string()));
            }
            Ok(())
        }

        fn reply(&self, id: TaskId, text: &str) -> Result<(), BoardError> {
            self.record(format!("reply {} {text}", id.0))?;
            check_message(text)
        }

        fn post_structured(&self, id: TaskId, schema: &str, json: &str) -> Result<(), BoardError> {
            self.record(format!("post_structured {} {schema} {json}", id.0))?;
            check_message(json)
        }

        fn complete(&self, id: TaskId, result: &str) -> Result<(), BoardError> {
            self.record(format!("complete {} {result}", id.0))
        }

        fn fail(&self, id: TaskId, reason: &str) -> Result<(), BoardError> {
            self.record(format!("fail {} {reason}", id.0))
        }

        fn block(&self, id: TaskId, reason: &str, kind: &str) -> Result<(), BoardError> {
            self.record(format!("block {} {kind} {reason}", id.0))
        }

        fn claim_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
            self.record(format!("claim_task {}", id.0))?;
            self.read_task(id)
        }

        fn renew(&self, id: TaskId) -> Result<TaskView, BoardError> {
            self.record(format!("renew {}", id.0))?;
            self.read_task(id)
        }

        fn release(&self, id: TaskId) -> Result<(), BoardError> {
            self.record(format!("release {}", id.0))
        }

        fn list_open(&self, subtree: &str) -> Result<Listing, BoardError> {
            self.record(format!("list_open {subtree}"))?;
            let closed = [TaskState::Completed, TaskState::Failed, TaskState::Canceled];
            let tasks = self.tasks.borrow();
            let open = tasks.iter().filter(|task| !closed.contains(&task.state));
            let listed = open.map(|view| ListedTask {
                view: view.clone(),
                channel: "work/fake".into(),
            });
            Ok(Listing {
                tasks: listed.collect(),
                cursor: Cursor(0),
                left_out: 0,
            })
        }

        fn list_labelled(
            &self,
            _: &str,
            key: &str,
            values: &[&str],
        ) -> Result<Listing, BoardError> {
            let tasks = self.tasks.borrow();
            let labelled = tasks.iter().filter(|task| {
                (task.labels.iter()).any(|(k, v)| k == key && values.contains(&v.as_str()))
            });
            let listed = labelled.map(|view| ListedTask {
                view: view.clone(),
                channel: "work/fake".into(),
            });
            Ok(Listing {
                tasks: listed.collect(),
                cursor: Cursor(0),
                left_out: 0,
            })
        }

        fn blocking_findings(&self, id: TaskId) -> Result<usize, BoardError> {
            self.read_task(id).map(|_| 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    use crate::test_support::write_executable;

    #[test]
    fn parses_a_recorded_task() {
        let v = parse_task_view(include_str!("../fixtures/board/read-task.json")).unwrap();
        assert_eq!(v.state, TaskState::Claimed);
        assert!(
            v.labels
                .iter()
                .any(|(k, v)| k == "role" && v == "implement")
        );
        assert!(!v.text.is_empty());
    }

    #[test]
    fn parses_a_recorded_claim() {
        let claim = parse_claim(include_str!("../fixtures/board/claim.json")).unwrap();
        assert!(claim.is_some());
    }

    #[test]
    fn reads_the_reason_from_a_recorded_failure() {
        let stderr = include_str!("../fixtures/board/claim-timeout.stderr.txt");
        assert_eq!(parse_failure_reason(stderr), Some("timeout".to_string()));
    }

    #[test]
    fn rejects_a_task_with_too_many_labels() {
        let labels: Vec<String> = (0..LABELS_MAX + 1)
            .map(|i| format!("\"k{i}\":\"v\""))
            .collect();
        let json = format!(
            "{{\"message\":{{\"id\":\"7\",\"text\":\"t\",\"labels\":{{{}}},\
             \"task\":{{\"state\":\"ready\",\"epoch\":0,\"needs\":[],\"claim\":null}},\
             \"version\":1}}}}",
            labels.join(",")
        );
        assert!(matches!(
            parse_task_view(&json),
            Err(BoardError::TooManyLabels)
        ));
    }

    #[test]
    fn rejects_json_that_is_not_a_task() {
        assert!(matches!(parse_task_view("{}"), Err(BoardError::Parse(_))));
        assert!(matches!(
            parse_task_view("not json"),
            Err(BoardError::Parse(_))
        ));
        assert!(matches!(parse_claim("{}"), Err(BoardError::Parse(_))));
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    const URL: &str = "http://127.0.0.1:4100/api/board";
    const GLOBAL: [&str; 5] = ["--json", "--url", URL, "--as", "opus-1"];

    fn expect(command: Command, words: &[&str]) {
        let mut expected = strings(words);
        expected.extend(strings(&GLOBAL));
        assert_eq!(command_args(&command, URL, "opus-1"), expected);
    }

    #[test]
    fn builds_read_and_claim_commands_as_recorded() {
        expect(Command::Read { id: TaskId(2) }, &["read", "2"]);
        let claim = Command::Claim {
            channel: "work/spike",
            role_label: "implement",
            wait_seconds: 5,
        };
        let words = [
            "tasks",
            "claim",
            "--ready",
            "work/spike",
            "--label",
            "role=implement",
            "--lease",
            "1h",
            "--wait",
            "5s",
        ];
        expect(claim, &words);
    }

    #[test]
    fn a_claim_that_does_not_wait_has_no_wait_option() {
        let claim = Command::Claim {
            channel: "work/spike",
            role_label: "implement",
            wait_seconds: 0,
        };
        let words = [
            "tasks",
            "claim",
            "--ready",
            "work/spike",
            "--label",
            "role=implement",
            "--lease",
            "1h",
        ];
        expect(claim, &words);
    }

    /// Words, then the global options, then `--` and the text.
    fn expect_with_text(command: Command, words: &[&str], text: &str) {
        let mut expected = strings(words);
        expected.extend(strings(&GLOBAL));
        expected.extend(strings(&["--", text]));
        assert_eq!(command_args(&command, URL, "opus-1"), expected);
        assert_eq!(command_stdin(&command), None);
    }

    #[test]
    fn builds_write_commands_as_recorded() {
        let id = TaskId(2);
        let label = Command::Label {
            id,
            key: "branch",
            value: "rider/x",
        };
        expect(label, &["label", "2", "branch=rider/x"]);
        assert_eq!(command_stdin(&label), None);
        let complete = Command::Complete { id, result: "done" };
        expect_with_text(complete, &["tasks", "complete", "2"], "done");
    }

    #[test]
    fn passes_text_that_starts_with_a_dash_safely() {
        let id = TaskId(2);
        let reply = Command::Reply {
            id,
            text: "-tail\n-of a log",
        };
        expect(reply, &["reply", "2", "-"]);
        assert_eq!(command_stdin(&reply), Some("-tail\n-of a log"));
        let post = Command::Post {
            id,
            schema: "rider-result@2",
            json: "{}",
        };
        expect_with_text(post, &["post", "2", "--schema", "rider-result@2"], "{}");
        let complete = Command::Complete {
            id,
            result: "-done\n  line",
        };
        expect_with_text(complete, &["tasks", "complete", "2"], "-done line");
        let fail = Command::Fail { id, reason: "-why" };
        expect_with_text(fail, &["tasks", "fail", "2"], "-why");
        let block = Command::Block {
            id: TaskId(4),
            reason: "-q?",
            kind: "needs_input",
        };
        expect_with_text(
            block,
            &["tasks", "block", "4", "--kind", "needs_input"],
            "-q?",
        );
    }

    #[test]
    fn collapses_text_to_one_bounded_line() {
        assert_eq!(one_line("  a\n\tb   c \r\n"), "a b c");
        let long = "x".repeat(RESULT_CHARS_MAX + 10);
        assert_eq!(one_line(&long).chars().count(), RESULT_CHARS_MAX);
        let multibyte = "é".repeat(RESULT_CHARS_MAX + 1);
        let line = one_line(&multibyte);
        assert_eq!(line.chars().count(), RESULT_CHARS_MAX);
        assert!(line.chars().all(|c| c == 'é'));
        let words = "ab ".repeat(RESULT_CHARS_MAX);
        let line = one_line(&words);
        assert!(line.chars().count() <= RESULT_CHARS_MAX);
        assert!(!line.ends_with(' '));
    }

    #[test]
    fn a_message_tail_keeps_the_end_from_a_whole_line_within_the_limit() {
        assert_eq!(message_tail("", 0), "");
        assert_eq!(message_tail("ab\ncd", 5), "ab\ncd");
        assert_eq!(message_tail("ab\ncd", 3), "cd");
        assert_eq!(message_tail("ab\ncd", 2), "cd");
        assert_eq!(message_tail("ab\ncd\n", 4), "cd\n");
        assert_eq!(message_tail("abcdef\n", 3), "ef\n");
        let chars_max = MESSAGE_CHARS_MAX - 1;
        let long = "日".repeat(MESSAGE_CHARS_MAX * 2);
        assert_eq!(message_tail(&long, chars_max), "日".repeat(chars_max));
        let lines: String = (0..MESSAGE_CHARS_MAX).map(|n| format!("é{n}ü\n")).collect();
        let tail = message_tail(&lines, chars_max);
        let dropped = &lines[..lines.len() - tail.len()];
        assert!(tail.chars().count() <= chars_max);
        assert!(
            tail.chars().count() > chars_max - 16,
            "at most one line is dropped"
        );
        assert!(tail.starts_with('é') && dropped.ends_with('\n'));
        assert!(tail.ends_with(&format!("é{}ü\n", MESSAGE_CHARS_MAX - 1)));
    }

    fn output(code: Option<i32>, stdout: &str, stderr: &str) -> Output {
        Output {
            code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn a_claim_that_times_out_or_finds_nothing_is_none() {
        let timeout = include_str!("../fixtures/board/claim-timeout.stderr.txt");
        let empty = include_str!("../fixtures/board/claim-timeout.json");
        assert!(matches!(
            claim_outcome(&output(Some(1), empty, timeout)),
            Ok(None)
        ));
        let nothing = r#"{"status":"error","reason":"nothing_ready","message":"Nothing."}"#;
        assert!(matches!(
            claim_outcome(&output(Some(1), "", nothing)),
            Ok(None)
        ));
        let claim = include_str!("../fixtures/board/claim.json");
        assert!(matches!(
            claim_outcome(&output(Some(0), claim, "")),
            Ok(Some(_))
        ));
    }

    #[test]
    fn any_other_failed_claim_is_an_exit_error() {
        let forbidden = r#"{"status":"error","reason":"forbidden","message":"no"}"#;
        let result = claim_outcome(&output(Some(1), "", forbidden));
        assert!(matches!(
            result,
            Err(BoardError::Exit { code: Some(1), .. })
        ));
        let timeout = include_str!("../fixtures/board/claim-timeout.stderr.txt");
        let killed = claim_outcome(&output(None, "", timeout));
        assert!(matches!(killed, Err(BoardError::Exit { code: None, .. })));
    }

    #[test]
    fn a_board_vb_cannot_reach_is_unreachable_for_every_call() {
        let unreachable = r#"{"status":"error","reason":"network_error","message":"Couldn't reach the board.","data":{"error":{"code":"unreachable","message":"the board didn't answer: fetch failed","status":502}}}"#;
        let failed = output(Some(1), "", unreachable);
        let is_unreachable = |error: Option<BoardError>| matches!(error, Some(BoardError::Unreachable { stderr }) if stderr == unreachable);
        assert!(is_unreachable(claim_outcome(&failed).err()));
        assert!(is_unreachable(lease_outcome(TaskId(2), &failed).err()));
        let done = Command::Complete {
            id: TaskId(2),
            result: "done",
        };
        assert!(is_unreachable(write_outcome(&done, &failed).err()));
    }

    #[test]
    fn reads_the_outcome_and_task_of_recorded_writes() {
        let complete = include_str!("../fixtures/board/after-complete.json");
        let (outcome, id) = parse_write(complete).unwrap();
        assert_eq!((outcome.as_str(), id), ("completed", TaskId(2)));
        let reply = include_str!("../fixtures/board/reply-attach.json");
        let (outcome, id) = parse_write(reply).unwrap();
        assert_eq!((outcome.as_str(), id), ("posted", TaskId(2)));
        let label = r#"{"message":{"id":"4"},"outcome":"labeled"}"#;
        let (outcome, id) = parse_write(label).unwrap();
        assert_eq!((outcome.as_str(), id), ("labeled", TaskId(4)));
        assert!(matches!(parse_write("{}"), Err(BoardError::Parse(_))));
    }

    #[test]
    fn a_write_must_report_the_right_outcome_for_the_right_task() {
        let stdout = include_str!("../fixtures/board/after-complete.json");
        let done = |id| Command::Complete { id, result: "done" };
        assert!(write_outcome(&done(TaskId(2)), &output(Some(0), stdout, "")).is_ok());
        let other = write_outcome(&done(TaskId(3)), &output(Some(0), stdout, ""));
        assert!(matches!(other, Err(BoardError::Parse(_))));
        let fail = Command::Fail {
            id: TaskId(2),
            reason: "why",
        };
        let wrong = write_outcome(&fail, &output(Some(0), stdout, ""));
        assert!(matches!(wrong, Err(BoardError::Parse(_))));
        let exited = write_outcome(&done(TaskId(2)), &output(Some(2), "", "bad"));
        assert!(matches!(
            exited,
            Err(BoardError::Exit { code: Some(2), .. })
        ));
    }

    #[test]
    fn output_over_the_bound_is_too_large() {
        let mut big = std::io::Cursor::new(vec![b'x'; OUTPUT_BYTES_MAX + 1]);
        let result = read_bounded(&mut big, "stdout");
        assert!(matches!(result, Err(BoardError::OutputTooLarge)));
        let mut fits = std::io::Cursor::new(b"{}".to_vec());
        assert_eq!(read_bounded(&mut fits, "stdout").unwrap(), "{}");
    }

    #[test]
    fn ids_are_canonical_decimal() {
        let json = |id: &str| {
            format!(
                "{{\"message\":{{\"id\":\"{id}\",\"text\":\"t\",\"labels\":{{}},\
                 \"task\":{{\"state\":\"ready\",\"epoch\":0,\"needs\":[],\"claim\":null}},\
                 \"version\":1}}}}"
            )
        };
        assert_eq!(parse_task_view(&json("17")).unwrap().id, TaskId(17));
        assert!(matches!(
            parse_task_view(&json("017")),
            Err(BoardError::Parse(_))
        ));
        assert!(matches!(
            parse_task_view(&json("+1")),
            Err(BoardError::Parse(_))
        ));
        assert!(matches!(
            parse_task_view(&json("")),
            Err(BoardError::Parse(_))
        ));
    }

    const HOLDER: &str = "local:user:zeke+opus-1";

    fn recorded(id: u64, text: &str, labels: &[(&str, &str)], state: TaskState) -> TaskView {
        TaskView {
            id: TaskId(id),
            text: text.to_string(),
            state,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            needs: Vec::new(),
            epoch: 1,
            claim: None,
            version: Cursor(0),
        }
    }

    fn versioned(view: TaskView, version: u64) -> TaskView {
        TaskView {
            version: Cursor(version),
            ..view
        }
    }

    fn held(mut view: TaskView, until: u64) -> TaskView {
        view.claim = Some(Claim {
            holder: HOLDER.to_string(),
            until: Timestamp(until),
        });
        view
    }

    #[test]
    fn parses_a_recorded_claim_by_id() {
        let claim = include_str!("../fixtures/board/claim-by-id.json");
        let labels = [("board.task", "claimed"), ("role", "implement")];
        let expected = held(
            versioned(recorded(2, "task a", &labels, TaskState::Claimed), 7),
            1_795_136_047_105,
        );
        assert_eq!(parse_lease(claim).unwrap(), expected);
        let view = lease_outcome(TaskId(2), &output(Some(0), claim, "")).unwrap();
        assert_eq!(view, expected);
        let other = lease_outcome(TaskId(3), &output(Some(0), claim, ""));
        assert!(matches!(other, Err(BoardError::Parse(_))), "{other:?}");
    }

    #[test]
    fn parses_a_recorded_renewal() {
        let renew = include_str!("../fixtures/board/renew.json");
        let expected = held(
            versioned(
                recorded(
                    12,
                    "task c",
                    &[("board.task", "claimed")],
                    TaskState::Claimed,
                ),
                15,
            ),
            1_795_136_077_438,
        );
        assert_eq!(parse_lease(renew).unwrap(), expected);
        let mismatched = renew.replace(r#""claim":{"epoch":1"#, r#""claim":{"epoch":2"#);
        assert!(matches!(
            parse_lease(&mismatched),
            Err(BoardError::Parse(_))
        ));
    }

    #[test]
    fn a_recorded_release_is_a_write_on_its_task() {
        let release = include_str!("../fixtures/board/release.json");
        let id = TaskId(2);
        let done = write_outcome(&Command::Release { id }, &output(Some(0), release, ""));
        assert!(matches!(done, Ok(())), "{done:?}");
        let (outcome, id) = parse_write(release).unwrap();
        assert_eq!((outcome.as_str(), id), ("released", TaskId(2)));
        let labels = [("board.task", "ready"), ("role", "implement")];
        #[derive(Deserialize)]
        struct Released {
            task: WireMessage,
        }
        let released: Released = parse(release).unwrap();
        let view = view_from_wire(released.task).unwrap();
        let expected = versioned(recorded(2, "task a", &labels, TaskState::Ready), 9);
        assert_eq!(view, expected);
    }

    #[test]
    fn parses_a_recorded_listing_with_needs_epochs_and_claims() {
        let list = include_str!("../fixtures/board/tasks-open.json");
        let claimed = [("board.task", "claimed"), ("role", "implement")];
        let a = held(
            versioned(recorded(2, "task a", &claimed, TaskState::Claimed), 8),
            1_795_133_349_261,
        );
        let waiting = [("board.task", "waiting"), ("role", "implement")];
        let mut b = versioned(recorded(4, "task b", &waiting, TaskState::Waiting), 5);
        b.needs = vec![TaskId(2)];
        b.epoch = 0;
        let listed = |view| ListedTask {
            view,
            channel: "work/scratch".into(),
        };
        let page = TaskPage {
            tasks: vec![listed(a), listed(b)],
            cursor: Cursor(8),
        };
        assert_eq!(parse_task_list(list).unwrap(), page);
    }

    /// A stand-in vb in a fresh directory, running `body`.
    fn stand_in(body: &str) -> (VbCli, TempDir) {
        let dir = TempDir::new().unwrap();
        let vb = dir.path().join("vb");
        write_executable(&vb, &format!("#!/bin/sh\n{body}"));
        let board = VbCli {
            vb,
            url: "http://127.0.0.1:1".into(),
            profile: "tester".into(),
        };
        (board, dir)
    }

    /// A stand-in vb that reads the recorded cursor `6` and lists open tasks `1..=count` in
    /// pages, as the board does: a full page's cursor is its last id, a short page's is the
    /// event cursor `9999`.
    fn listing_of(count: usize) -> (VbCli, TempDir) {
        let task = concat!(
            r#"{"message":{"id":"%d","channel":"work/t","text":"t","labels":{},"#,
            r#""task":{"state":"ready","epoch":0,"needs":[],"claim":null},"version":%d}}"#
        );
        let read = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/board/read-cursor.json"
        );
        let body = format!(
            r#"[ "$1" = read ] && exec cat '{read}'
after=0
while [ $# -gt 0 ]; do [ "$1" = --after ] && after=$2; shift; done
last=$((after + {TASKS_PAGE_MAX}))
[ $last -gt {count} ] && last={count}
printf '{{"tasks":['
id=$((after + 1)); sep=
while [ $id -le $last ]; do printf "$sep"; printf '{task}' $id $id; sep=,; id=$((id + 1)); done
cursor=9999
[ $((last - after)) -eq {TASKS_PAGE_MAX} ] && cursor=$last
printf '],"cursor":"%s"}}\n' $cursor
"#
        );
        stand_in(&body)
    }

    #[test]
    fn a_listing_returns_the_board_cursor() {
        let (board, _dir) = listing_of(3);
        let listing = board.list_open("work/");
        let listing = listing.unwrap();
        let ids: Vec<u64> = listing.tasks.iter().map(|task| task.view.id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!((listing.cursor, listing.left_out), (Cursor(6), 0));
    }

    #[test]
    fn a_listing_pages_up_to_the_tracked_tasks_and_counts_the_rest() {
        let count = TASKS_MAX + 201;
        let (board, _dir) = listing_of(count);
        let listing = board.list_open("work/");
        let listing = listing.unwrap();
        let ids: Vec<u64> = listing.tasks.iter().map(|task| task.view.id.0).collect();
        let tracked: Vec<u64> = (1..=TASKS_MAX as u64).collect();
        assert_eq!(ids, tracked);
        assert_eq!((listing.cursor, listing.left_out), (Cursor(6), 201));
    }

    #[test]
    fn a_listing_past_the_page_bound_is_too_many_tasks() {
        let (board, _dir) = listing_of(TASKS_PAGE_MAX * LISTING_PAGES_MAX + 1);
        let listing = board.list_open("work/");
        assert!(
            matches!(listing, Err(BoardError::TooManyTasks)),
            "{listing:?}"
        );
    }

    #[test]
    fn a_fenced_write_refused_for_the_claim_is_a_lost_claim() {
        let id = TaskId(2);
        let complete = Command::Complete { id, result: "done" };
        let recorded = [
            include_str!("../fixtures/board/claim-lost.stderr.txt"),
            include_str!("../fixtures/board/claim-not-held.stderr.txt"),
        ];
        for stderr in recorded {
            let written = write_outcome(&complete, &output(Some(1), "", stderr));
            assert!(matches!(
                written,
                Err(BoardError::ClaimLost { id: TaskId(2) })
            ));
            let renewed = lease_outcome(id, &output(Some(1), "", stderr));
            assert!(matches!(
                renewed,
                Err(BoardError::ClaimLost { id: TaskId(2) })
            ));
            let released = write_outcome(&Command::Release { id }, &output(Some(1), "", stderr));
            assert!(matches!(
                released,
                Err(BoardError::ClaimLost { id: TaskId(2) })
            ));
            let killed = write_outcome(&complete, &output(None, "", stderr));
            assert!(matches!(killed, Err(BoardError::Exit { code: None, .. })));
        }
        let forbidden = r#"{"status":"error","reason":"forbidden","message":"no"}"#;
        let other = write_outcome(&complete, &output(Some(1), "", forbidden));
        assert!(matches!(other, Err(BoardError::Exit { code: Some(1), .. })));
    }

    #[test]
    fn times_parse_only_in_the_boards_own_shape() {
        let parse = |text: &str| parse_timestamp(text).ok();
        assert_eq!(parse("1970-01-01T00:00:00.000Z"), Some(Timestamp(0)));
        assert_eq!(
            parse("2026-11-19T20:38:37.720Z"),
            Some(Timestamp(1_795_120_717_720))
        );
        assert_eq!(
            parse("2024-02-29T23:59:59.999Z"),
            Some(Timestamp(1_709_251_199_999))
        );
        let malformed = [
            "2026-11-19T20:38:37Z",
            "2026-11-19T20:38:37.720+00:00",
            "2026-13-19T20:38:37.720Z",
            "2025-02-29T20:38:37.720Z",
            "2026-11-19T24:00:00.000Z",
            "2026-11-19T23:59:60.000Z",
            "1969-12-31T23:59:59.999Z",
            "2026-11-19 20:38:37.720Z",
            "2026-11-+9T20:38:37.720Z",
            "2026-11-1éT20:38:37.72Z",
        ];
        for text in malformed {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn builds_lease_and_listing_commands_as_recorded() {
        let id = TaskId(2);
        expect(
            Command::ClaimId { id },
            &["tasks", "claim", "2", "--lease", "1h"],
        );
        expect(
            Command::Renew { id },
            &["tasks", "renew", "2", "--lease", "1h"],
        );
        expect(Command::Release { id }, &["tasks", "release", "2"]);
        let cursor = Command::Cursor { subtree: "work/" };
        expect(cursor, &["read", "work/", "--limit", "1"]);
        let list = Command::List {
            subtree: "work/",
            state: "all",
            label: Some("base=3|#3"),
            after: Some(Cursor(9)),
        };
        let limit = ["--limit", "200", "--label", "base=3|#3", "--after", "9"];
        expect(
            list,
            &[&["tasks", "work/", "--state", "all"][..], &limit].concat(),
        );
    }

    /// Upper bound on the runs of one timeout test whose `sh` is too slow to write its pid.
    const TIMEOUT_RUNS_MAX: usize = 5;

    /// Runs `read_task` against a stand-in vb made of `body`, which writes the pid to kill to
    /// `$0.pid`, and checks the call times out at the limit and that pid is gone afterwards.
    fn times_out_and_kills(body: &str) {
        let limit = Duration::from_secs(u64::from(CALL_SECONDS));
        for _ in 0..TIMEOUT_RUNS_MAX {
            let (board, dir) = stand_in(body);
            let started = Instant::now();
            let result = board.read_task(TaskId(7));
            let took = started.elapsed();
            assert!(
                matches!(
                    result,
                    Err(BoardError::TimedOut {
                        call: "read",
                        seconds: CALL_SECONDS
                    })
                ),
                "{result:?}"
            );
            assert!(
                took >= limit && took < limit + Duration::from_secs(1),
                "{took:?}"
            );
            // On a loaded machine `sh` may be killed before it writes the pid; that run proves
            // nothing, and everything it started went with its group.
            let Ok(pid) = fs::read_to_string(dir.path().join("vb.pid")) else {
                continue;
            };
            let pid = Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
            // A killed descendant stays a zombie until its new parent reaps it.
            let gone = (0..100).any(|_| {
                thread::sleep(Duration::from_millis(20));
                rustix::process::test_kill_process(pid) == Err(Errno::SRCH)
            });
            assert!(gone, "{} is still running", pid.as_raw_pid());
            return;
        }
        panic!("{body}: no run of {TIMEOUT_RUNS_MAX} wrote its pid before the limit");
    }

    #[test]
    fn a_vb_that_never_exits_is_killed_at_the_limit() {
        times_out_and_kills("echo $$ > \"$0.pid\"\nexec sleep 3600\n");
    }

    #[test]
    fn a_descendant_holding_the_output_of_an_exited_vb_is_killed_at_the_limit() {
        times_out_and_kills("sleep 120 &\necho $! > \"$0.pid\"\nexit 0\n");
    }

    #[test]
    fn a_descendant_of_a_waiting_vb_is_killed_at_the_limit() {
        times_out_and_kills("sleep 120 &\necho $! > \"$0.pid\"\nwait\n");
    }

    #[test]
    fn a_descendant_is_killed_with_no_kill_command_on_path() {
        const CHILD: &str = "RIDER_BOARD_TEST_EMPTY_PATH";
        let name = "board::tests::a_descendant_is_killed_with_no_kill_command_on_path";
        if std::env::var_os(CHILD).is_some() {
            assert!(std::env::var_os("PATH").is_some_and(|path| !path.is_empty()));
            let body = "/bin/sleep 120 &\necho $! > \"$0.pid\"\nwait\n";
            times_out_and_kills(body);
            return;
        }
        let empty = TempDir::new().unwrap();
        let ran = Process::new(std::env::current_exe().unwrap())
            .args(["--exact", "--test-threads", "1", "--", name])
            .env("PATH", empty.path())
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            ran.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
    }
}
