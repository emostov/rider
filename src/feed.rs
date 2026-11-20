//! The board's event feed: a `vb tail --json` child whose lines become typed events.

use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::mem;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
use serde_json::{Map, Value};

use crate::board::{LABELS_MAX, TaskId, TaskState, VbCli, kill_group};

/// Upper bound on one line of the feed, its newline excluded.
pub const FEED_LINE_BYTES_MAX: usize = 1024 * 1024;

const DIGITS_MAX: usize = 20;

/// Upper bound on how long one `next_item` waits before it reports the feed idle.
pub const FEED_IDLE_SECONDS_MAX: u32 = 3600;

/// What one read may take: a full line and its newline.
const READ_BYTES_MAX: u64 = FEED_LINE_BYTES_MAX as u64 + 1;

/// Lines read ahead of the caller.
const LINES_QUEUED_MAX: usize = 4;

/// Reads over the life of one feed; past it the feed ends and is started again.
const READS_MAX: u64 = 1 << 40;

/// How long the child's exit and the close of its stdout may lag each other.
const END_GRACE: Duration = Duration::from_secs(2);

const POLL_MILLIS: u32 = 20;
const POLL_INTERVAL: Duration = Duration::from_millis(POLL_MILLIS as u64);
const POLLS_MAX: u32 = FEED_IDLE_SECONDS_MAX * 1000 / POLL_MILLIS;

/// A position in the board's event stream, as `vb tail --after` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor(pub u64);

/// The event types the feed names; any other type arrives as `FeedEvent::Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    TaskCreated,
    LabelsChanged,
    TaskClaimed,
    TaskReleased,
    TaskExpired,
    TaskBlocked,
    TaskUnblocked,
    TaskReopened,
    TaskCompleted,
    TaskFailed,
    TaskCanceled,
    MessagePosted,
}

impl EventKind {
    /// Whether the event happens only to a task, never to an ordinary message or a reply.
    fn is_task_only(self) -> bool {
        !matches!(self, EventKind::MessagePosted | EventKind::LabelsChanged)
    }
}

/// One event of a known type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardEvent {
    pub cursor: Cursor,
    pub kind: EventKind,
    /// The task the event is about; for a message or label event, its thread's root.
    pub task: TaskId,
    pub channel: String,
    /// The labels of the event's message: the task's for a task event, the reply's for a reply.
    pub labels: Vec<(String, String)>,
    /// The task's state after the event; `None` when the message is not a task, as a reply is.
    pub state: Option<TaskState>,
    /// The board's actor id; `None` for what the board does itself, such as a lease expiring.
    pub actor: Option<String>,
}

/// One line of the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEvent {
    Known(BoardEvent),
    /// An event whose type the feed does not name.
    Other {
        cursor: Cursor,
    },
}

/// What the feed gives next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedItem {
    Event(FeedEvent),
    /// The child exited; `code` is `None` when a signal ended it.
    Exited {
        code: Option<i32>,
    },
    /// Nothing arrived for `FEED_IDLE_SECONDS_MAX`; the feed is still following.
    Idle,
}

#[derive(Debug)]
pub enum FeedError {
    Spawn(String),
    Io(String),
    /// A line ran past `FEED_LINE_BYTES_MAX` bytes.
    LineTooLong,
    /// A line that is not an event the board prints, with what was wrong.
    Parse(String),
    TooManyLabels,
    /// The feed already reported its exit.
    Ended,
}

impl fmt::Display for FeedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedError::Spawn(detail) => write!(formatter, "could not start vb tail: {detail}"),
            FeedError::Io(detail) => write!(formatter, "reading vb tail failed: {detail}"),
            FeedError::LineTooLong => write!(
                formatter,
                "vb tail printed a line over {FEED_LINE_BYTES_MAX} bytes"
            ),
            FeedError::Parse(detail) => write!(formatter, "vb tail printed {detail}"),
            FeedError::TooManyLabels => {
                write!(formatter, "a feed event has over {LABELS_MAX} labels")
            }
            FeedError::Ended => write!(formatter, "the feed already reported its exit"),
        }
    }
}

/// A message from the thread that reads the child's stdout.
enum Received {
    Line(Vec<u8>),
    Failed(FeedError),
    /// Stdout closed, or could no longer be read.
    End,
}

/// A running `vb tail` in its own process group. Dropping it kills the group. After an
/// error, drop it and start again from the last cursor seen.
pub struct FeedReader {
    child: Child,
    lines: Receiver<Received>,
    drained: bool,
    /// The child has exited. It stays unreaped until `finish`, so its pid still names its
    /// group and cannot be reused before the group is killed.
    exited: bool,
    /// When stdout closed or the child exited, whichever came first.
    ending_since: Option<Instant>,
    ended: bool,
}

impl FeedReader {
    /// Starts following every event on the board after `after`.
    pub fn start(cli: &VbCli, after: Cursor) -> Result<FeedReader, FeedError> {
        assert!(!cli.url.is_empty(), "the board URL is never empty");
        assert!(!cli.profile.is_empty(), "the profile is never empty");
        let after = after.0.to_string();
        let args = [
            "tail",
            "--after",
            &after,
            "--json",
            "--url",
            &cli.url,
            "--as",
            &cli.profile,
        ];
        let mut child = Command::new(&cli.vb)
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| FeedError::Spawn(format!("{}: {error}", cli.vb.display())))?;
        let (sender, lines) = sync_channel(LINES_QUEUED_MAX);
        let started = child
            .stdout
            .take()
            .ok_or_else(|| "no stdout pipe".to_string());
        let started = started.and_then(|stdout| {
            let stdout = BufReader::with_capacity(64 * 1024, stdout);
            let builder = thread::Builder::new().name("vb-tail".into());
            let spawned = builder.spawn(move || read_lines(stdout, &sender));
            spawned.map_err(|error| format!("starting the reader thread: {error}"))
        });
        if let Err(detail) = started {
            let _ = kill_group(&mut child);
            return Err(FeedError::Spawn(detail));
        }
        Ok(FeedReader {
            child,
            lines,
            drained: false,
            exited: false,
            ending_since: None,
            ended: false,
        })
    }

    /// Blocks until the next event, until the child exits, or until the feed has been idle
    /// for `FEED_IDLE_SECONDS_MAX`. The exit is reported once; every later call is `Ended`.
    pub fn next_item(&mut self) -> Result<FeedItem, FeedError> {
        if self.ended {
            return Err(FeedError::Ended);
        }
        for _ in 0..POLLS_MAX {
            if self.drained {
                thread::sleep(POLL_INTERVAL);
            } else {
                match self.lines.recv_timeout(POLL_INTERVAL) {
                    Ok(Received::Line(line)) => return line_event(line),
                    Ok(Received::Failed(error)) => return Err(error),
                    Ok(Received::End) | Err(RecvTimeoutError::Disconnected) => self.drained = true,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            if !self.exited {
                let child = WaitId::Pid(Pid::from_child(&self.child));
                let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
                let polled = waitid(child, options);
                self.exited = polled
                    .map_err(|error| FeedError::Io(error.to_string()))?
                    .is_some();
            }
            if !self.drained && !self.exited {
                continue;
            }
            // A descendant may hold stdout after `vb` exits, or `vb` may close it and keep
            // running: either way the other end gets `END_GRACE`, then the group is killed.
            let since = *self.ending_since.get_or_insert_with(Instant::now);
            if self.drained && self.exited || since.elapsed() >= END_GRACE {
                return self.finish();
            }
        }
        assert!(!self.ended);
        Ok(FeedItem::Idle)
    }

    fn finish(&mut self) -> Result<FeedItem, FeedError> {
        assert!(!self.ended, "the exit is reported once");
        self.ended = true;
        let killed = kill_group(&mut self.child);
        // Darwin refuses (EPERM) to signal a group whose only member is the exited, unreaped
        // child, so once that child exited and stdout closed, a refusal leaves nothing behind.
        if !(self.exited && self.drained) {
            killed.map_err(|error| FeedError::Io(error.to_string()))?;
        }
        let status = self.child.wait();
        let status = status.map_err(|error| FeedError::Io(error.to_string()))?;
        Ok(FeedItem::Exited {
            code: status.code(),
        })
    }
}

impl Drop for FeedReader {
    fn drop(&mut self) {
        if !self.ended {
            // Nothing more can be done about a group that cannot be signalled.
            let _ = kill_group(&mut self.child);
        }
    }
}

fn line_event(line: Vec<u8>) -> Result<FeedItem, FeedError> {
    assert!(line.len() <= FEED_LINE_BYTES_MAX);
    let line =
        String::from_utf8(line).map_err(|_| FeedError::Parse("a line that is not UTF-8".into()))?;
    parse_event(&line).map(FeedItem::Event)
}

/// Sends each line of `stdout` without its newline. An over-long line is reported once and
/// the rest of it discarded. Ends when stdout closes or nobody is listening.
fn read_lines(mut stdout: BufReader<ChildStdout>, sender: &SyncSender<Received>) {
    let mut line: Vec<u8> = Vec::new();
    let mut skipping = false;
    for _ in 0..READS_MAX {
        line.clear();
        let read = (&mut stdout)
            .take(READ_BYTES_MAX)
            .read_until(b'\n', &mut line);
        let received = match read {
            Ok(0) => break,
            Ok(read) => {
                assert_eq!(read, line.len());
                assert!(line.len() <= FEED_LINE_BYTES_MAX + 1);
                if line.last() == Some(&b'\n') {
                    line.pop();
                    let skipped = skipping;
                    skipping = false;
                    if skipped {
                        continue;
                    }
                    Received::Line(mem::take(&mut line))
                } else if skipping {
                    continue;
                } else if line.len() > FEED_LINE_BYTES_MAX {
                    skipping = true;
                    Received::Failed(FeedError::LineTooLong)
                } else {
                    Received::Failed(FeedError::Parse("a last line cut off at exit".into()))
                }
            }
            Err(error) => {
                let _ = sender.send(Received::Failed(FeedError::Io(error.to_string())));
                break;
            }
        };
        if sender.send(received).is_err() {
            return;
        }
    }
    let _ = sender.send(Received::End);
}

/// One line of `vb tail --json`: `{cursor, type, payload, message, actor}`.
pub fn parse_event(line: &str) -> Result<FeedEvent, FeedError> {
    if line.len() > FEED_LINE_BYTES_MAX {
        return Err(FeedError::LineTooLong);
    }
    let value: Value = serde_json::from_str(line)
        .map_err(|error| FeedError::Parse(format!("a line that is not JSON: {error}")))?;
    let Value::Object(event) = value else {
        return Err(FeedError::Parse("a line that is not a JSON object".into()));
    };
    let cursor = parse_number(string_field(&event, "cursor")?, "cursor").map(Cursor)?;
    let kind = match string_field(&event, "type")? {
        "task.created" => EventKind::TaskCreated,
        "labels.changed" => EventKind::LabelsChanged,
        "task.claimed" => EventKind::TaskClaimed,
        "task.released" => EventKind::TaskReleased,
        "task.expired" => EventKind::TaskExpired,
        "task.blocked" => EventKind::TaskBlocked,
        "task.unblocked" => EventKind::TaskUnblocked,
        "task.reopened" => EventKind::TaskReopened,
        "task.completed" => EventKind::TaskCompleted,
        "task.failed" => EventKind::TaskFailed,
        "task.canceled" => EventKind::TaskCanceled,
        "message.posted" => EventKind::MessagePosted,
        _ => return Ok(FeedEvent::Other { cursor }),
    };
    let message = event.get("message").and_then(Value::as_object);
    let message = message.ok_or_else(|| field_error(cursor, "no message object"))?;
    let known = BoardEvent {
        cursor,
        kind,
        task: parse_task(message, kind, cursor)?,
        channel: string_field(message, "channel")?.to_string(),
        labels: parse_labels(message)?,
        state: parse_state(message, cursor)?,
        actor: parse_actor(&event, cursor)?,
    };
    if kind.is_task_only() && known.state.is_none() {
        return Err(field_error(cursor, "a task event with no message.task"));
    }
    assert!(known.labels.len() <= LABELS_MAX);
    assert_eq!(known.cursor, cursor);
    Ok(FeedEvent::Known(known))
}

fn field_error(cursor: Cursor, detail: &str) -> FeedError {
    FeedError::Parse(format!("event {}: {detail}", cursor.0))
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, FeedError> {
    let value = object.get(key).and_then(Value::as_str);
    value.ok_or_else(|| FeedError::Parse(format!("an event with no string {key}")))
}

/// A cursor or id as the board prints it: canonical decimal, no sign and no leading zeros.
fn parse_number(text: &str, name: &str) -> Result<u64, FeedError> {
    let canonical = !text.is_empty()
        && text.len() <= DIGITS_MAX
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    let number = text.parse::<u64>().ok().filter(|_| canonical);
    let number = number.ok_or_else(|| FeedError::Parse(format!("{name} {text:?}")))?;
    assert_eq!(number.to_string(), text);
    Ok(number)
}

/// A reply's task is its thread's root; any other message is its own root.
fn parse_task(
    message: &Map<String, Value>,
    kind: EventKind,
    cursor: Cursor,
) -> Result<TaskId, FeedError> {
    let id = parse_number(string_field(message, "id")?, "message.id")?;
    let task = match message.get("re") {
        Some(Value::Null) => id,
        Some(Value::String(root)) => parse_number(root, "message.re")?,
        _ => {
            return Err(field_error(
                cursor,
                "message.re is neither null nor a string",
            ));
        }
    };
    if kind.is_task_only() && task != id {
        return Err(field_error(cursor, "a task event on a reply"));
    }
    Ok(TaskId(task))
}

fn parse_labels(message: &Map<String, Value>) -> Result<Vec<(String, String)>, FeedError> {
    let labels = message.get("labels").and_then(Value::as_object);
    let labels = labels.ok_or_else(|| FeedError::Parse("no message.labels object".into()))?;
    if labels.len() > LABELS_MAX {
        return Err(FeedError::TooManyLabels);
    }
    let mut result: Vec<(String, String)> = Vec::with_capacity(labels.len());
    for (key, value) in labels.iter().take(LABELS_MAX) {
        let value = value.as_str();
        let value = value.ok_or_else(|| FeedError::Parse(format!("label {key} not a string")))?;
        result.push((key.clone(), value.to_string()));
    }
    assert_eq!(result.len(), labels.len(), "no label was dropped");
    Ok(result)
}

fn parse_state(
    message: &Map<String, Value>,
    cursor: Cursor,
) -> Result<Option<TaskState>, FeedError> {
    let task = match message.get("task") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(task)) => task,
        Some(_) => return Err(field_error(cursor, "message.task is not an object")),
    };
    let state = task.get("state").and_then(Value::as_str);
    let state = state.ok_or_else(|| field_error(cursor, "no string message.task.state"))?;
    let parsed = match state {
        "waiting" => TaskState::Waiting,
        "ready" => TaskState::Ready,
        "claimed" => TaskState::Claimed,
        "blocked" => TaskState::Blocked,
        "completed" => TaskState::Completed,
        "failed" => TaskState::Failed,
        "canceled" => TaskState::Canceled,
        _ => {
            return Err(field_error(
                cursor,
                &format!("unknown task state {state:?}"),
            ));
        }
    };
    Ok(Some(parsed))
}

fn parse_actor(event: &Map<String, Value>, cursor: Cursor) -> Result<Option<String>, FeedError> {
    // The board leaves `actor` out of what it does itself, such as expiring a lease.
    let actor = match event.get("actor") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(actor)) => actor,
        Some(_) => return Err(field_error(cursor, "actor is not an object")),
    };
    let id = actor.get("id").and_then(Value::as_str);
    let id = id.ok_or_else(|| field_error(cursor, "no string actor.id"))?;
    if id.is_empty() {
        return Err(field_error(cursor, "an empty actor.id"));
    }
    Ok(Some(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const PERSON: &str = "local:user:zeke";
    const WORKER: &str = "local:user:zeke+w-1";

    fn fixture(name: &str) -> FeedEvent {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/board")
            .join(name);
        let text = fs::read_to_string(&path).unwrap();
        parse_event(text.trim_end_matches('\n')).unwrap()
    }

    fn known(
        cursor: u64,
        kind: EventKind,
        task: u64,
        labels: &[(&str, &str)],
        state: Option<TaskState>,
        actor: Option<&str>,
    ) -> FeedEvent {
        FeedEvent::Known(BoardEvent {
            cursor: Cursor(cursor),
            kind,
            task: TaskId(task),
            channel: "work/scratch".into(),
            labels: labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            state,
            actor: actor.map(str::to_string),
        })
    }

    #[test]
    fn parses_each_recorded_event() {
        use EventKind::*;
        use TaskState::*;
        let (p, w) = (Some(PERSON), Some(WORKER));
        let x = ("branch", "rider/x");
        let role = ("role", "implement");
        let at = |state| [("board.task", state), x, role];
        let ready = at("ready");
        let canceled = [("board.task", "canceled"), role];
        #[rustfmt::skip]
        let cases = [
            ("task-created", 3, TaskCreated, 2, &[("board.task", "ready"), role][..], Ready, p),
            ("labels-changed", 6, LabelsChanged, 2, &ready, Ready, p),
            ("task-claimed", 8, TaskClaimed, 2, &at("claimed"), Claimed, w),
            ("task-released", 9, TaskReleased, 2, &ready, Ready, w),
            ("task-expired", 11, TaskExpired, 2, &ready, Ready, None),
            ("task-blocked", 13, TaskBlocked, 2, &at("blocked"), Blocked, w),
            ("task-unblocked", 15, TaskUnblocked, 2, &ready, Ready, p),
            ("task-failed", 17, TaskFailed, 2, &at("failed"), Failed, w),
            ("task-reopened", 18, TaskReopened, 2, &ready, Ready, p),
            ("task-completed", 20, TaskCompleted, 2, &at("completed"), Completed, w),
            ("task-canceled", 22, TaskCanceled, 4, &canceled, Canceled, p),
        ];
        for (name, cursor, kind, task, labels, state, actor) in cases {
            let expected = known(cursor, kind, task, labels, Some(state), actor);
            assert_eq!(fixture(&format!("tail-{name}.json")), expected, "{name}");
        }
        let reply = known(14, MessagePosted, 2, &[], None, p);
        assert_eq!(fixture("tail-message-posted-reply.json"), reply);
        let other = FeedEvent::Other { cursor: Cursor(21) };
        assert_eq!(fixture("tail-task-ready.json"), other);
    }

    #[test]
    fn rejects_a_task_event_without_its_task() {
        let line = r#"{"cursor":"5","type":"task.claimed","actor":null,
            "message":{"id":"2","re":null,"channel":"c","labels":{}}}"#;
        let result = parse_event(&line.replace('\n', ""));
        assert!(matches!(result, Err(FeedError::Parse(_))), "{result:?}");
    }

    /// A reader over a stand-in `vb` whose script is `body`.
    fn reader(name: &str, body: &str) -> (FeedReader, PathBuf) {
        let unique = format!("rider-feed-{name}-{}", std::process::id());
        let dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(&dir).unwrap();
        let vb = dir.join("vb");
        fs::write(&vb, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&vb, fs::Permissions::from_mode(0o755)).unwrap();
        let cli = VbCli {
            vb,
            url: "http://127.0.0.1:1".into(),
            profile: "tester".into(),
        };
        (FeedReader::start(&cli, Cursor(0)).unwrap(), dir)
    }

    #[test]
    fn rejects_a_line_over_the_bound() {
        let body = format!(
            "head -c {} /dev/zero | tr '\\0' x\necho\n",
            FEED_LINE_BYTES_MAX + 1
        );
        let (mut feed, dir) = reader("long", &body);
        let result = feed.next_item();
        fs::remove_dir_all(dir).unwrap();
        assert!(matches!(result, Err(FeedError::LineTooLong)), "{result:?}");
    }

    #[test]
    fn reads_a_line_at_the_bound() {
        let body = format!(
            "head -c {} /dev/zero | tr '\\0' x\necho\n",
            FEED_LINE_BYTES_MAX
        );
        let (mut feed, dir) = reader("bound", &body);
        let result = feed.next_item();
        fs::remove_dir_all(dir).unwrap();
        assert!(matches!(result, Err(FeedError::Parse(_))), "{result:?}");
    }

    #[test]
    fn yields_events_then_reports_the_exit() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/board");
        let body = format!(
            "cat '{}' '{}'\nexit 3\n",
            path.join("tail-task-expired.json").display(),
            path.join("tail-task-ready.json").display()
        );
        let (mut feed, dir) = reader("exit", &body);
        let expired = fixture("tail-task-expired.json");
        assert_eq!(feed.next_item().unwrap(), FeedItem::Event(expired));
        let other = FeedEvent::Other { cursor: Cursor(21) };
        assert_eq!(feed.next_item().unwrap(), FeedItem::Event(other));
        assert_eq!(
            feed.next_item().unwrap(),
            FeedItem::Exited { code: Some(3) }
        );
        assert!(matches!(feed.next_item(), Err(FeedError::Ended)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn skips_the_rest_of_an_over_long_line() {
        let body = format!(
            "head -c {} /dev/zero | tr '\\0' x\nprintf '{}\\n{}\\n'\n",
            FEED_LINE_BYTES_MAX + 1,
            r#"{"cursor":"999","type":"future.event"}"#,
            r#"{"cursor":"8","type":"future.event"}"#,
        );
        let (mut feed, dir) = reader("skip", &body);
        let first = feed.next_item();
        let second = feed.next_item();
        fs::remove_dir_all(dir).unwrap();
        assert!(matches!(first, Err(FeedError::LineTooLong)), "{first:?}");
        let other = FeedEvent::Other { cursor: Cursor(8) };
        assert_eq!(second.unwrap(), FeedItem::Event(other));
    }

    #[test]
    fn parses_labels_changed_on_a_message_and_a_reply() {
        let event = |cursor, task, labels: &[(&str, &str)]| {
            let FeedEvent::Known(mut event) = known(
                cursor,
                EventKind::LabelsChanged,
                task,
                labels,
                None,
                Some(PERSON),
            ) else {
                panic!("known always builds a known event");
            };
            event.channel = "review/feed".into();
            FeedEvent::Known(event)
        };
        let message = event(3, 2, &[("status", "ready")]);
        assert_eq!(fixture("tail-labels-changed-message.json"), message);
        let reply = event(5, 2, &[("note", "seen")]);
        assert_eq!(fixture("tail-labels-changed-reply.json"), reply);
    }

    #[test]
    fn reports_a_signalled_exit_once() {
        let (mut feed, dir) = reader("signal", "kill -9 $$\n");
        let exited = feed.next_item();
        let after = feed.next_item();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(exited.unwrap(), FeedItem::Exited { code: None });
        assert!(matches!(after, Err(FeedError::Ended)), "{after:?}");
    }

    #[test]
    fn reports_the_exit_while_a_descendant_holds_stdout() {
        let (mut feed, dir) = reader("descendant", "sleep 30 &\nexit 3\n");
        let started = Instant::now();
        let exited = feed.next_item();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(exited.unwrap(), FeedItem::Exited { code: Some(3) });
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn dropping_the_feed_kills_its_process_group() {
        let unique = format!("rider-feed-group-pid-{}", std::process::id());
        let pid_file = std::env::temp_dir().join(unique);
        let body = format!(
            "sleep 30 >/dev/null &\necho $! > '{}'\ncat '{}'\nsleep 30\n",
            pid_file.display(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/board/tail-task-ready.json")
                .display()
        );
        let (mut feed, dir) = reader("group", &body);
        let other = FeedEvent::Other { cursor: Cursor(21) };
        assert_eq!(feed.next_item().unwrap(), FeedItem::Event(other));
        drop(feed);
        let pid = fs::read_to_string(&pid_file).unwrap();
        fs::remove_file(&pid_file).unwrap();
        fs::remove_dir_all(dir).unwrap();
        let alive = || {
            Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !alive()
        });
        assert!(gone, "the descendant {} outlived the feed", pid.trim());
    }

    #[test]
    fn rejects_an_over_long_string() {
        let result = parse_event(&" ".repeat(FEED_LINE_BYTES_MAX + 1));
        assert!(matches!(result, Err(FeedError::LineTooLong)), "{result:?}");
    }
}
