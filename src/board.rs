//! The Board trait, and its one implementation over the `vb` command-line program.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Command as Process, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process_group};
use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer, IgnoredAny};

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
const VERBS: [&str; 6] = ["read", "tasks", "label", "reply", "post", "schemas"];
pub const RIDER_SCHEMAS: [(&str, &str); 2] = [
    (
        "rider-result@3",
        include_str!("../skills/ride-swarm/schemas/rider-result@3.json"),
    ),
    (
        "rider-finding@1",
        include_str!("../skills/ride-swarm/schemas/rider-finding.json"),
    ),
];
const SCHEMAS_LISTED_MAX: usize = 1024;
/// The number of global arguments: `--json --url <url> --as <profile>`.
const GLOBAL_ARGS_LEN: usize = 5;
pub const CREATE_LABELS_MAX: usize = 4;
pub const CREATE_NEEDS_MAX: usize = 1;
pub const SET_NEEDS_MAX: usize = 12;
const _: () = assert!(3 + SET_NEEDS_MAX + GLOBAL_ARGS_LEN <= ARGS_MAX);
const _: () =
    assert!(6 + 2 * CREATE_LABELS_MAX + 1 + CREATE_NEEDS_MAX + GLOBAL_ARGS_LEN + 2 <= ARGS_MAX);
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
pub const RERUN_OF_LABEL: &str = "rerun_of";
/// The reasons a fenced write gives when its claim is over or was never held.
const CLAIM_LOST_REASONS: [&str; 2] = ["claim_lost", "claim_not_held"];
/// The reasons `vb` gives when it could not reach the board at all.
const UNREACHABLE_REASONS: [&str; 1] = ["network_error"];
/// The length of a time as the board prints it: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
const TIMESTAMP_LEN: usize = 24;
/// The channel that holds the board's identity markers, in the board's own data.
pub const IDENTITY_CHANNEL: &str = "rider/identity";
/// What an identity marker's text starts with; random hex follows.
const MARKER_PREFIX: &str = "rider board ";
/// The random bytes in one identity marker.
const MARKER_BYTES: usize = 16;
/// Upper bound on the threads read from the identity channel; more is an error.
const IDENTITY_THREADS_MAX: usize = 50;
/// The failure reason of a read of a channel that does not exist yet.
const CHANNEL_NOT_FOUND: &str = "channel_not_found";

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
    pub channel: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedTask {
    pub view: TaskView,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amendment {
    pub id: Cursor,
    pub profile: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub state: TaskState,
    pub epoch: u64,
    pub cursor: Cursor,
    pub amendments: Vec<Amendment>,
    pub others: Vec<Amendment>,
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

impl BoardError {
    pub fn call_failed(&self) -> bool {
        matches!(
            self,
            BoardError::Spawn(_)
                | BoardError::Io(_)
                | BoardError::TimedOut { .. }
                | BoardError::Unreachable { .. }
        )
    }
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
    /// The board's identity: the oldest marker in its own data, posted first when it has none,
    /// so replacing or wiping the board's data changes it.
    fn identity(&self) -> Result<String, BoardError>;
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
    fn thread(&self, id: TaskId) -> Result<Thread, BoardError>;
    fn schema_ids(&self) -> Result<Vec<String>, BoardError>;
    fn create_schema(&self, id: &str, json: &str) -> Result<(), BoardError>;
    fn set_needs(&self, id: TaskId, needs: &[TaskId]) -> Result<(), BoardError>;
    fn create_task(
        &self,
        channel: &str,
        text: &str,
        labels: &[(&str, &str)],
        needs: &[TaskId],
        key: &str,
    ) -> Result<TaskId, BoardError>;
}

pub fn register_schemas(board: &dyn Board) -> Result<Vec<&'static str>, BoardError> {
    let known = board.schema_ids()?;
    let mut registered = Vec::with_capacity(RIDER_SCHEMAS.len());
    for (id, json) in RIDER_SCHEMAS {
        if !known.iter().any(|known| known == id) {
            board.create_schema(id, json)?;
            registered.push(id);
        }
    }
    assert!(registered.len() <= RIDER_SCHEMAS.len());
    Ok(registered)
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
    Channel {
        channel: &'a str,
    },
    PostRoot {
        channel: &'a str,
        text: &'a str,
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
    Schemas,
    CreateSchema {
        id: &'a str,
        json: &'a str,
    },
    Needs {
        id: TaskId,
        needs: &'a [TaskId],
    },
    Create {
        channel: &'a str,
        text: &'a str,
        labels: &'a [(&'a str, &'a str)],
        needs: &'a [TaskId],
        key: &'a str,
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
        Command::Channel { channel } => {
            let limit = IDENTITY_THREADS_MAX.to_string();
            (to_strings(&["read", channel, "--limit", &limit]), None)
        }
        Command::PostRoot { channel, text } => (to_strings(&["post", channel]), Some(text.into())),
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
        Command::Schemas => (to_strings(&["schemas", "list"]), None),
        // The schema's JSON goes on stdin; `--file -` tells `vb` to read it there.
        Command::CreateSchema { id, .. } => {
            (to_strings(&["schemas", "create", id, "--file", "-"]), None)
        }
        Command::Needs { id, needs } => {
            let mut words = to_strings(&["tasks", "needs", &id.0.to_string()]);
            words.extend(needs.iter().map(|need| need.0.to_string()));
            (words, None)
        }
        // The task's text goes on stdin; `-` tells `vb` to read it there.
        Command::Create {
            channel,
            labels,
            needs,
            key,
            ..
        } => {
            let mut words = to_strings(&["tasks", "create", channel, "-"]);
            for (name, value) in labels {
                words.extend(["--label".to_string(), format!("{name}={value}")]);
            }
            if !needs.is_empty() {
                words.push("--needs".to_string());
                words.extend(needs.iter().map(|need| need.0.to_string()));
            }
            words.extend(to_strings(&["--idempotency-key", key]));
            (words, None)
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

/// The text `command` sends on stdin, if any: a reply's text or a schema's JSON, either of
/// which may span many lines.
fn command_stdin<'a>(command: &Command<'a>) -> Option<&'a str> {
    let stdin = match *command {
        Command::Reply { text, .. } => Some(text),
        Command::CreateSchema { json, .. } => Some(json),
        Command::Create { text, .. } => Some(text),
        _ => None,
    };
    assert!(
        stdin.is_none()
            || matches!(
                command,
                Command::Reply { .. } | Command::CreateSchema { .. } | Command::Create { .. }
            )
    );
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
        Command::Channel { .. } => ("read", 0),
        Command::PostRoot { .. } => ("post", 0),
        Command::List { .. } => ("tasks", 0),
        Command::Label { .. } => ("label", 0),
        Command::Reply { .. } => ("reply", 0),
        Command::Post { .. } => ("post", 0),
        Command::Complete { .. } => ("tasks complete", 0),
        Command::Fail { .. } => ("tasks fail", 0),
        Command::Block { .. } => ("tasks block", 0),
        Command::Schemas => ("schemas list", 0),
        Command::CreateSchema { .. } => ("schemas create", 0),
        Command::Create { .. } => ("tasks create", 0),
        Command::Needs { .. } => ("tasks needs", 0),
    };
    assert!(
        wait_seconds <= WAIT_SECONDS_MAX,
        "a claim waits at most an hour"
    );
    let seconds = BOARD_CALL_SECONDS_MAX + wait_seconds;
    assert!(seconds >= BOARD_CALL_SECONDS_MAX);
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
        | Command::Channel { .. }
        | Command::PostRoot { .. }
        | Command::List { .. }
        | Command::Schemas
        | Command::CreateSchema { .. }
        | Command::Create { .. }
        | Command::Needs { .. } => None,
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
    channel: String,
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

#[derive(Deserialize)]
struct WireTaskThread {
    message: WireMessage,
    replies: Vec<WireTextReply>,
    cursor: String,
}

#[derive(Deserialize)]
struct WireThreadAuthors {
    message: WireAuthoredTask,
}

#[derive(Deserialize)]
struct WireAuthoredTask {
    by: WireBy,
    task: WireOwnedTask,
}

#[derive(Deserialize)]
struct WireOwnedTask {
    owner: String,
}

#[derive(Deserialize)]
struct WireTextReply {
    id: String,
    text: String,
    by: WireBy,
}

#[derive(Deserialize)]
struct WireBy {
    actor: String,
    principal: String,
    #[serde(deserialize_with = "nullable")]
    profile: Option<String>,
}

/// The replies of `vb read <id> --json`, each with its structured blocks.
#[derive(Deserialize)]
struct WireReplies {
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

#[derive(Deserialize)]
struct WireSchemas {
    schemas: Vec<WireSchema>,
}

#[derive(Deserialize)]
struct WireSchema {
    id: String,
}

/// A write's output: `{task, outcome}` or `{message, outcome}`.
#[derive(Deserialize)]
struct WireWrite {
    outcome: String,
    task: Option<WireTaskRef>,
    message: Option<WireMessageRef>,
}

/// A root's `message` in a channel read: only its ID and text are used.
#[derive(Deserialize)]
struct WireRoot {
    id: String,
    text: String,
}

#[derive(Deserialize)]
struct WireThread {
    message: WireRoot,
}

/// `vb read <channel> --json`: `{threads: [{message, ...}], cursor, older}`, `older` null on
/// the last page.
#[derive(Deserialize)]
struct WireThreads {
    threads: Vec<WireThread>,
    #[serde(deserialize_with = "nullable")]
    older: Option<IgnoredAny>,
}

/// The text of the oldest identity marker in a read of the identity channel, if any. Roots
/// that are not markers are ignored; a channel with more than one page is an error.
pub fn parse_oldest_marker(json: &str) -> Result<Option<String>, BoardError> {
    let read: WireThreads = parse(json)?;
    if read.older.is_some() || read.threads.len() > IDENTITY_THREADS_MAX {
        return Err(BoardError::Parse(format!(
            "more than {IDENTITY_THREADS_MAX} threads in {IDENTITY_CHANNEL}"
        )));
    }
    let mut oldest: Option<(TaskId, String)> = None;
    for thread in read.threads {
        let WireRoot { id, text } = thread.message;
        let id = parse_id_text(&id)?;
        if is_marker(&text) && oldest.as_ref().is_none_or(|(first, _)| id.0 < first.0) {
            oldest = Some((id, text));
        }
    }
    assert!(oldest.as_ref().is_none_or(|(_, text)| is_marker(text)));
    Ok(oldest.map(|(_, text)| text))
}

fn is_marker(text: &str) -> bool {
    let hex = text.strip_prefix(MARKER_PREFIX).unwrap_or_default();
    let digits = MARKER_BYTES * 2;
    hex.len() == digits
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A new identity marker: the prefix, then random bytes in hex.
fn new_marker() -> Result<String, BoardError> {
    let mut bytes = [0u8; MARKER_BYTES];
    let read = File::open("/dev/urandom").and_then(|mut random| random.read_exact(&mut bytes));
    read.map_err(|error| BoardError::Io(format!("reading /dev/urandom: {error}")))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let marker = format!("{MARKER_PREFIX}{hex}");
    assert!(is_marker(&marker));
    Ok(marker)
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
        channel,
        text,
        labels,
        task,
        version,
    } = message;
    if channel.is_empty() {
        return Err(BoardError::Parse("a task in an empty channel".into()));
    }
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
        channel,
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
    let thread: WireReplies = parse(json)?;
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

pub fn parse_thread(id: TaskId, json: &str) -> Result<Thread, BoardError> {
    let thread: WireTaskThread = parse(json)?;
    let authors: WireThreadAuthors = parse(json)?;
    let (owner, creator) = (authors.message.task.owner, authors.message.by.actor);
    if owner.is_empty() || creator.is_empty() {
        return Err(BoardError::Parse(
            "a task without its owner or creator".into(),
        ));
    }
    if thread.replies.len() > REPLIES_MAX {
        let count = thread.replies.len();
        return Err(BoardError::Parse(format!(
            "a thread of {count} replies, over {REPLIES_MAX}"
        )));
    }
    let view = view_from_wire(thread.message)?;
    if view.id != id {
        return Err(BoardError::Parse(format!(
            "asked for {id:?}, read {:?}",
            view.id
        )));
    }
    let cursor = Cursor(parse_id_text(&thread.cursor)?.0);
    let mut amendments: Vec<Amendment> = Vec::new();
    let mut others: Vec<Amendment> = Vec::new();
    let made_by_rider = view.labels.iter().any(|(key, _)| key == RERUN_OF_LABEL);
    for reply in thread.replies {
        let by = &reply.by;
        // The board verifies `actor` and `principal`; a profile is only a name its login picks.
        let by_creator = by.actor == creator && !made_by_rider;
        let amends = by.principal == owner && (by.actor == owner || by_creator);
        let id = Cursor(parse_id_text(&reply.id)?.0);
        let (profile, text) = (reply.by.profile, reply.text);
        let to = if amends { &mut amendments } else { &mut others };
        to.push(Amendment { id, profile, text });
    }
    assert!(amendments.len() + others.len() <= REPLIES_MAX);
    Ok(Thread {
        state: view.state,
        epoch: view.epoch,
        cursor,
        amendments,
        others,
    })
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
            let view = view_from_wire(listed.message.message)?;
            Ok(ListedTask { view })
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
    assert!(seconds >= BOARD_CALL_SECONDS_MAX);
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

    /// The oldest identity marker on the board; none when the identity channel does not exist.
    fn oldest_marker(&self) -> Result<Option<String>, BoardError> {
        let channel = IDENTITY_CHANNEL;
        let output = self.run(&Command::Channel { channel })?;
        if output.code == Some(0) {
            return parse_oldest_marker(&output.stdout);
        }
        let reason = output
            .code
            .and_then(|_| parse_failure_reason(&output.stderr));
        if reason.as_deref() == Some(CHANNEL_NOT_FOUND) {
            return Ok(None);
        }
        Err(BoardError::Exit {
            code: output.code,
            stderr: output.stderr,
        })
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
    fn identity(&self) -> Result<String, BoardError> {
        assert!(!self.url.is_empty() && !self.profile.is_empty());
        if let Some(marker) = self.oldest_marker()? {
            return Ok(marker);
        }
        let marker = new_marker()?;
        let channel = IDENTITY_CHANNEL;
        let output = self.run(&Command::PostRoot {
            channel,
            text: &marker,
        })?;
        if output.code != Some(0) {
            return Err(BoardError::Exit {
                code: output.code,
                stderr: output.stderr,
            });
        }
        let write: WireWrite = parse(&output.stdout)?;
        if write.outcome != "posted" {
            return Err(BoardError::Parse(format!(
                "posting a marker: {}",
                write.outcome
            )));
        }
        // Another rider may have posted its marker first: the oldest is the board's.
        let oldest = self.oldest_marker()?;
        let oldest = oldest.ok_or_else(|| BoardError::Parse("the posted marker is gone".into()))?;
        assert!(is_marker(&oldest));
        Ok(oldest)
    }

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

    fn thread(&self, id: TaskId) -> Result<Thread, BoardError> {
        parse_thread(id, &self.read(&Command::Read { id })?)
    }

    fn schema_ids(&self) -> Result<Vec<String>, BoardError> {
        let listed: WireSchemas = parse(&self.read(&Command::Schemas)?)?;
        if listed.schemas.len() > SCHEMAS_LISTED_MAX {
            return Err(BoardError::Parse(format!(
                "more than {SCHEMAS_LISTED_MAX} schemas listed"
            )));
        }
        let ids: Vec<String> = listed.schemas.into_iter().map(|schema| schema.id).collect();
        assert!(ids.len() <= SCHEMAS_LISTED_MAX);
        Ok(ids)
    }

    fn create_schema(&self, id: &str, json: &str) -> Result<(), BoardError> {
        assert!(
            id.contains('@') && !id.starts_with('-'),
            "a schema is name@version"
        );
        assert!(!json.is_empty() && json.len() <= OUTPUT_BYTES_MAX);
        self.read(&Command::CreateSchema { id, json }).map(drop)
    }

    fn create_task(
        &self,
        channel: &str,
        text: &str,
        labels: &[(&str, &str)],
        needs: &[TaskId],
        key: &str,
    ) -> Result<TaskId, BoardError> {
        assert!(!channel.is_empty() && !channel.starts_with('-'));
        assert!(!key.is_empty() && !key.starts_with('-'));
        assert!(labels.len() <= CREATE_LABELS_MAX && needs.len() <= CREATE_NEEDS_MAX);
        // `vb` drops one final newline from text read on stdin.
        let text = format!("{text}\n");
        let command = Command::Create {
            channel,
            text: &text,
            labels,
            needs,
            key,
        };
        let (_, id) = parse_write(&self.read(&command)?)?;
        Ok(id)
    }

    fn set_needs(&self, id: TaskId, needs: &[TaskId]) -> Result<(), BoardError> {
        assert!(!needs.is_empty() && needs.len() <= SET_NEEDS_MAX);
        assert!(!needs.contains(&id));
        self.read(&Command::Needs { id, needs }).map(drop)
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
