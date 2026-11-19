//! The Board trait, and its one implementation over the `vb` command-line program.

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command as Process, Stdio};
use std::thread::{self, JoinHandle};

use serde_json::{Map, Value};

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

#[derive(Debug, Clone)]
pub struct TaskView {
    pub id: TaskId,
    pub text: String,
    pub state: TaskState,
    /// At most `LABELS_MAX` entries.
    pub labels: Vec<(String, String)>,
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
    /// `vb` printed more than `OUTPUT_BYTES_MAX` bytes on one stream.
    OutputTooLarge,
    /// Reading from or writing to `vb` failed.
    Io(String),
    /// A message of `chars` characters, over `MESSAGE_CHARS_MAX`, was refused before `vb` ran.
    MessageTooLong {
        chars: usize,
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

impl TaskState {
    fn from_board(state: &str) -> Option<TaskState> {
        assert!(state.len() <= OUTPUT_BYTES_MAX);
        let parsed = match state {
            "waiting" => Some(TaskState::Waiting),
            "ready" => Some(TaskState::Ready),
            "claimed" => Some(TaskState::Claimed),
            "blocked" => Some(TaskState::Blocked),
            "completed" => Some(TaskState::Completed),
            "failed" => Some(TaskState::Failed),
            "canceled" => Some(TaskState::Canceled),
            _ => None,
        };
        assert!(parsed.is_none() || state.bytes().all(|byte| byte.is_ascii_lowercase()));
        parsed
    }
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

/// The argument list for `command`: its words as the commands in docs/board-observed.md were
/// run, then the global `--json --url <url> --as <profile>`, then `--` and any free text, so
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

/// The outcome a successful run of `command` reports, and the task it acts on, for writes.
fn expected_write(command: &Command) -> Option<(&'static str, TaskId)> {
    match *command {
        Command::Read { .. } | Command::Claim { .. } => None,
        Command::Label { id, .. } => Some(("labeled", id)),
        Command::Reply { id, .. } | Command::Post { id, .. } => Some(("posted", id)),
        Command::Complete { id, .. } => Some(("completed", id)),
        Command::Fail { id, .. } => Some(("failed", id)),
        Command::Block { id, .. } => Some(("blocked", id)),
    }
}

fn parse_object(json: &str) -> Result<Map<String, Value>, BoardError> {
    assert!(
        json.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    let value: Value = serde_json::from_str(json.trim())
        .map_err(|error| BoardError::Parse(format!("not JSON: {error}")))?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(BoardError::Parse("not a JSON object".into())),
    }
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

fn parse_id(message: &Map<String, Value>, key: &str) -> Result<TaskId, BoardError> {
    assert!(key == "id" || key == "re");
    let id = message.get(key).and_then(Value::as_str);
    let id = id.ok_or_else(|| BoardError::Parse(format!("no string {key}")))?;
    parse_id_text(id)
}

fn parse_labels(message: &Map<String, Value>) -> Result<Vec<(String, String)>, BoardError> {
    let labels = message.get("labels").and_then(Value::as_object);
    let labels = labels.ok_or_else(|| BoardError::Parse("no labels object".into()))?;
    if labels.len() > LABELS_MAX {
        return Err(BoardError::TooManyLabels);
    }
    let mut result: Vec<(String, String)> = Vec::with_capacity(labels.len());
    for (key, value) in labels.iter().take(LABELS_MAX) {
        let value = value.as_str();
        let value = value.ok_or_else(|| BoardError::Parse(format!("label {key} not a string")))?;
        result.push((key.clone(), value.to_string()));
    }
    assert_eq!(result.len(), labels.len(), "no label was dropped");
    Ok(result)
}

/// A task from a message object, as `read` prints under `message` and `claim` under `task`.
fn parse_message(message: &Map<String, Value>) -> Result<TaskView, BoardError> {
    let id = parse_id(message, "id")?;
    let text = message.get("text").and_then(Value::as_str);
    let text = text
        .ok_or_else(|| BoardError::Parse("no text".into()))?
        .to_string();
    let state = message
        .get("task")
        .and_then(|task| task.get("state"))
        .and_then(Value::as_str);
    let state = state.ok_or_else(|| BoardError::Parse("not a task: no task.state".into()))?;
    let state = TaskState::from_board(state)
        .ok_or_else(|| BoardError::Parse(format!("unknown task state {state:?}")))?;
    let labels = parse_labels(message)?;
    assert!(text.len() < OUTPUT_BYTES_MAX);
    Ok(TaskView {
        id,
        text,
        state,
        labels,
    })
}

/// A task from the output of `vb read <id> --json`: `{message, replies, cursor, schedules}`.
pub fn parse_task_view(json: &str) -> Result<TaskView, BoardError> {
    let object = parse_object(json)?;
    let message = object.get("message").and_then(Value::as_object);
    let message = message.ok_or_else(|| BoardError::Parse("no message object".into()))?;
    let view = parse_message(message)?;
    assert!(view.labels.len() <= LABELS_MAX);
    Ok(view)
}

/// The claimed task from the output of a successful `vb tasks claim --json`: `{task, claim}`.
/// `None` when the board printed a null task.
pub fn parse_claim(json: &str) -> Result<Option<TaskId>, BoardError> {
    let object = parse_object(json)?;
    let task = object
        .get("task")
        .ok_or_else(|| BoardError::Parse("no task".into()))?;
    if task.is_null() {
        return Ok(None);
    }
    let task = task
        .as_object()
        .ok_or_else(|| BoardError::Parse("task not an object".into()))?;
    let view = parse_message(task)?;
    if view.state != TaskState::Claimed {
        return Err(BoardError::Parse(format!(
            "claimed task is {:?}",
            view.state
        )));
    }
    Ok(Some(view.id))
}

/// The `reason` of a failed run, from the JSON it printed: `{status: "error", reason, message}`.
pub fn parse_failure_reason(stderr: &str) -> Option<String> {
    assert!(
        stderr.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    let object = parse_object(stderr).ok()?;
    if object.get("status").and_then(Value::as_str) == Some("ok") {
        return None;
    }
    let reason = object.get("reason")?.as_str()?.to_string();
    Some(reason)
}

/// The `outcome` of a write and the task it acted on. Task writes print `{task, outcome}`;
/// `label` prints `{message: {id}, outcome}`; `reply` and `post` print the new reply as
/// `{message, outcome}`, whose `re` is the task.
fn parse_write(json: &str) -> Result<(String, TaskId), BoardError> {
    let object = parse_object(json)?;
    let outcome = object.get("outcome").and_then(Value::as_str);
    let outcome = outcome.filter(|outcome| !outcome.is_empty());
    let outcome = outcome.ok_or_else(|| BoardError::Parse("no outcome".into()))?;
    let id = if let Some(task) = object.get("task").and_then(Value::as_object) {
        parse_id(task, "id")?
    } else {
        let message = object.get("message").and_then(Value::as_object);
        let message = message.ok_or_else(|| BoardError::Parse("no task or message".into()))?;
        let is_reply = message.get("re").is_some_and(|re| !re.is_null());
        parse_id(message, if is_reply { "re" } else { "id" })?
    };
    assert!(!outcome.is_empty());
    Ok((outcome.to_string(), id))
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
    Err(BoardError::Exit {
        code: output.code,
        stderr: output.stderr.clone(),
    })
}

/// Whether a write's run succeeded with the outcome `command` expects, on its task.
fn write_outcome(command: &Command, output: &Output) -> Result<(), BoardError> {
    let expected = expected_write(command);
    let (expected_outcome, expected_id) =
        expected.ok_or_else(|| BoardError::Parse("not a write".into()))?;
    assert!(!expected_outcome.is_empty());
    if output.code != Some(0) {
        return Err(BoardError::Exit {
            code: output.code,
            stderr: output.stderr.clone(),
        });
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

/// Feeds `input` to `child`, reads both of its streams to the end, then waits for it. Each
/// stream but stdout has its own thread, so no full pipe can stall `vb`.
fn collect(child: &mut Child, input: Option<String>) -> Result<Output, BoardError> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| BoardError::Spawn("no stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| BoardError::Spawn("no stderr".into()))?;
    let writer = match (input, child.stdin.take()) {
        (Some(text), Some(stdin)) => Some(start_worker("stdin", move || feed(stdin, text))?),
        (None, None) => None,
        _ => {
            return Err(BoardError::Spawn(
                "stdin is not set up for the input".into(),
            ));
        }
    };
    let reader = start_worker("stderr", move || read_bounded(&mut stderr, "stderr"))?;
    let stdout = read_bounded(&mut stdout, "stdout");
    if stdout.is_err() {
        // Killing can fail only when the child already exited, which is the goal anyway.
        let _ = child.kill();
    }
    let status = child.wait();
    let status = status.map_err(|error| BoardError::Io(format!("waiting: {error}")))?;
    let stderr = finish_worker(reader, "stderr");
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
        let mut child = Process::new(&self.vb)
            .args(&args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| BoardError::Spawn(format!("{}: {error}", self.vb.display())))?;
        let output = collect(&mut child, input);
        if output.is_err() {
            // A thread could not start, or a stream broke: do not leave `vb` running.
            let _ = child.kill();
            let _ = child.wait();
        }
        output
    }

    fn write(&self, command: &Command) -> Result<(), BoardError> {
        assert!(
            expected_write(command).is_some(),
            "only writes report an outcome"
        );
        let output = self.run(command)?;
        write_outcome(command, &output)
    }
}

impl Board for VbCli {
    fn read_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
        assert!(!self.url.is_empty() && !self.profile.is_empty());
        let output = self.run(&Command::Read { id })?;
        if output.code != Some(0) {
            return Err(BoardError::Exit {
                code: output.code,
                stderr: output.stderr,
            });
        }
        let view = parse_task_view(&output.stdout)?;
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
}

/// A board held in memory, for tests in this crate.
#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::{Board, BoardError, TaskId, TaskState, TaskView, check_message};

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
            self.record(format!("set_label {} {key}={value}", id.0))
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
             \"task\":{{\"state\":\"ready\"}}}}}}",
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
            schema: "rider-result@1",
            json: "{}",
        };
        expect_with_text(post, &["post", "2", "--schema", "rider-result@1"], "{}");
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
                 \"task\":{{\"state\":\"ready\"}}}}}}"
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
}
