//! The board's event feed: a `vb tail --json` child whose lines become typed events.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::mem;
use std::process::{ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde::Deserialize;

use crate::board::{
    BoardError, LABELS_MAX, TaskId, TaskState, VbCli, WireState, kill_group, nullable,
};

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

/// How long stdout may stay open after the group is killed: every writer in the group is
/// dead, so only the reader catching up, or a writer outside the group, can take longer.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(10);

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
    /// The board made a waiting task ready because its needs were met.
    TaskReady,
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
    /// The process group could not be killed, so something it started may still run.
    Kill(String),
    /// Stdout stayed open `DRAIN_AFTER_KILL` after the group was killed.
    Undrained,
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
            FeedError::Kill(detail) => write!(formatter, "killing vb tail failed: {detail}"),
            FeedError::Undrained => write!(
                formatter,
                "vb tail's stdout stayed open {DRAIN_AFTER_KILL:?} after its group was killed"
            ),
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

/// How a feed reads its child's stdout.
type ReadLines = fn(BufReader<ChildStdout>, &SyncSender<Received>);

/// A running `vb tail` in its own process group. Dropping it kills the group. After an
/// error, drop it and start again from the last cursor seen.
pub struct FeedReader {
    child: Box<dyn ChildWrapper>,
    /// The child's pid, which names its group.
    leader: Pid,
    /// The child's group while the child is unreaped; locked across every reap.
    group: Arc<Mutex<Option<Pid>>>,
    lines: Receiver<Received>,
    drained: bool,
    /// The child has exited. It stays unreaped until `finish`, so its pid still names its
    /// group and cannot be reused before the group is killed.
    exited: bool,
    /// When stdout closed or the child exited, whichever came first.
    ending_since: Option<Instant>,
    /// When the group was killed; the child is reaped and what it wrote is still being read.
    killed: Option<Instant>,
    /// The group was killed and nothing is left in it, so dropping the feed signals nothing.
    group_gone: bool,
    ended: bool,
}

impl FeedReader {
    /// Starts following every event on the board after `after`.
    pub fn start(cli: &VbCli, after: Cursor) -> Result<FeedReader, FeedError> {
        FeedReader::start_with(cli, after, read_lines)
    }

    fn start_with(cli: &VbCli, after: Cursor, read: ReadLines) -> Result<FeedReader, FeedError> {
        assert!(!cli.url.is_empty(), "the board URL is never empty");
        assert!(!cli.profile.is_empty(), "the profile is never empty");
        assert!(!cli.vb.as_os_str().is_empty(), "the vb path is never empty");
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
        let mut process = Command::new(&cli.vb);
        process
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let spawned = CommandWrap::from(process)
            .wrap(ProcessGroup::leader())
            .spawn();
        let mut child =
            spawned.map_err(|error| FeedError::Spawn(format!("{}: {error}", cli.vb.display())))?;
        let (sender, lines) = sync_channel(LINES_QUEUED_MAX);
        let leader = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
        let started = match (leader, child.stdout().take()) {
            (Some(leader), Some(stdout)) => {
                let stdout = BufReader::with_capacity(64 * 1024, stdout);
                let builder = thread::Builder::new().name("vb-tail".into());
                let spawned = builder.spawn(move || read(stdout, &sender));
                let spawned = spawned.map(|_| leader);
                spawned.map_err(|error| format!("starting the reader thread: {error}"))
            }
            (None, _) => Err(format!("pid {} is out of range", child.id())),
            (_, None) => Err("no stdout pipe".to_string()),
        };
        let leader = match started {
            Ok(leader) => leader,
            Err(detail) => {
                let _ = kill_group(child.as_mut());
                return Err(FeedError::Spawn(detail));
            }
        };
        let group = Arc::new(Mutex::new(Some(leader)));
        Ok(FeedReader {
            child,
            leader,
            group,
            lines,
            drained: false,
            exited: false,
            ending_since: None,
            killed: None,
            group_gone: false,
            ended: false,
        })
    }

    /// Blocks until the next event, until the child exits, or until the feed has been idle
    /// for `FEED_IDLE_SECONDS_MAX`. Every line read before the exit comes first; the exit is
    /// reported once and every later call is `Ended`.
    pub fn next_item(&mut self) -> Result<FeedItem, FeedError> {
        if self.ended {
            return Err(FeedError::Ended);
        }
        assert!(
            !self.group_gone || self.killed.is_some(),
            "only a kill empties the group"
        );
        assert!(self.killed.is_none() || self.ending_since.is_some());
        for _ in 0..POLLS_MAX {
            // The exit is checked before every read, so a descendant that keeps stdout busy
            // cannot postpone it. A killed child is already reaped and no longer polled.
            if !self.exited && self.killed.is_none() {
                let child = WaitId::Pid(self.leader);
                let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
                let polled = waitid(child, options);
                self.exited = polled
                    .map_err(|error| FeedError::Io(error.to_string()))?
                    .is_some();
            }
            if self.drained || self.exited {
                // A descendant may hold stdout after `vb` exits, or `vb` may close it and keep
                // running: either way the other end gets `END_GRACE` from the first of the
                // two, which later output does not extend, then the group is killed.
                let since = *self.ending_since.get_or_insert_with(Instant::now);
                if self.killed.is_none() && since.elapsed() >= END_GRACE {
                    self.kill()?;
                }
            }
            if self.drained {
                if self.exited || self.killed.is_some() {
                    return self.finish();
                }
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            let wait = match self.killed {
                Some(killed) => {
                    let left = DRAIN_AFTER_KILL.saturating_sub(killed.elapsed());
                    if left.is_zero() {
                        return Err(FeedError::Undrained);
                    }
                    left.min(POLL_INTERVAL)
                }
                None => POLL_INTERVAL,
            };
            match self.lines.recv_timeout(wait) {
                Ok(Received::Line(line)) => return line_event(line),
                Ok(Received::Failed(error)) => return Err(error),
                Ok(Received::End) | Err(RecvTimeoutError::Disconnected) => self.drained = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        assert!(!self.ended);
        Ok(FeedItem::Idle)
    }

    /// A handle that ends this feed from another thread.
    pub fn stopper(&self) -> FeedStop {
        assert!(
            !self.ended,
            "a feed that reported its exit has nothing to stop"
        );
        let stop = FeedStop {
            group: Arc::clone(&self.group),
        };
        assert!(Arc::strong_count(&self.group) >= 2);
        stop
    }

    /// Kills the group and reaps the child, holding the group's lock so no `FeedStop`
    /// signals the group's id once the child is reaped and the id may be reused.
    fn kill_group(&mut self) -> Result<(), BoardError> {
        let mut group = lock(&self.group);
        assert!(
            group.is_some() || self.killed.is_some(),
            "only a kill reaps the child"
        );
        let killed = kill_group(self.child.as_mut());
        *group = None;
        killed
    }

    /// Kills the group once, leaving the lines it already wrote to be read.
    fn kill(&mut self) -> Result<(), FeedError> {
        assert!(self.killed.is_none(), "the group is killed once");
        assert!(!self.ended);
        assert!(
            !self.group_gone,
            "the group is not gone before it is killed"
        );
        self.killed = Some(Instant::now());
        let killed = self.kill_group();
        self.group_gone = killed.is_ok();
        killed.map_err(|error| FeedError::Kill(error.to_string()))
    }

    fn finish(&mut self) -> Result<FeedItem, FeedError> {
        assert!(!self.ended, "the exit is reported once");
        assert!(self.drained);
        if self.killed.is_none() {
            self.kill()?;
        }
        assert!(self.killed.is_some());
        self.ended = true;
        let status = self.child.try_wait();
        let status = status.map_err(|error| FeedError::Io(error.to_string()))?;
        let status = status.ok_or_else(|| FeedError::Io("vb tail outlived its kill".into()))?;
        Ok(FeedItem::Exited {
            code: status.code(),
        })
    }
}

impl Drop for FeedReader {
    fn drop(&mut self) {
        // A group that is gone is not signalled again: its id may already name another group.
        if !self.group_gone {
            // Nothing more can be done about a group that cannot be signalled.
            let _ = self.kill_group();
        }
    }
}

/// Ends a feed from another thread: its `next_item` then reports the exit.
pub struct FeedStop {
    group: Arc<Mutex<Option<Pid>>>,
}

impl FeedStop {
    /// Kills the feed's process group, unless the feed has already reaped its child.
    pub fn stop(&self) -> Result<(), FeedError> {
        let group = lock(&self.group);
        let Some(pid) = *group else {
            return Ok(());
        };
        assert!(pid.as_raw_pid() > 1, "never signal the caller's own group");
        match kill_process_group(pid, Signal::KILL) {
            // Darwin refuses (EPERM) to signal a group whose only member is the exited,
            // unreaped leader: nothing in it is left to stop.
            Ok(()) | Err(Errno::SRCH | Errno::PERM) => Ok(()),
            Err(error) => Err(FeedError::Kill(format!(
                "group {}: {error}",
                pid.as_raw_pid()
            ))),
        }
    }
}

fn lock(group: &Mutex<Option<Pid>>) -> MutexGuard<'_, Option<Pid>> {
    // The lock guards no invariant a panicking holder could break halfway.
    group.lock().unwrap_or_else(PoisonError::into_inner)
}

fn line_event(line: Vec<u8>) -> Result<FeedItem, FeedError> {
    assert!(line.len() <= FEED_LINE_BYTES_MAX);
    let line =
        String::from_utf8(line).map_err(|_| FeedError::Parse("a line that is not UTF-8".into()))?;
    let event = parse_event(&line)?;
    assert!(matches!(&event, FeedEvent::Other { .. }) || line.contains("\"message\""));
    Ok(FeedItem::Event(event))
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

/// The fields every line of `vb tail --json` carries, whatever its type.
#[derive(Deserialize)]
struct WireHead {
    cursor: String,
    #[serde(rename = "type")]
    kind: String,
}

/// `message.task` of a task's event.
#[derive(Deserialize)]
struct WireTask {
    state: WireState,
}

/// The message an event is about.
#[derive(Deserialize)]
struct WireMessage {
    id: String,
    #[serde(deserialize_with = "nullable")]
    re: Option<String>,
    channel: String,
    labels: BTreeMap<String, String>,
    /// Absent or null when the message is not a task.
    task: Option<WireTask>,
}

#[derive(Deserialize)]
struct WireActor {
    id: String,
}

/// The rest of an event of a type the feed names.
#[derive(Deserialize)]
struct WireEvent {
    message: WireMessage,
    /// The board leaves `actor` out of what it does itself, such as expiring a lease.
    actor: Option<WireActor>,
}

/// One line of `vb tail --json`: `{cursor, type, payload, message, actor}`.
pub fn parse_event(line: &str) -> Result<FeedEvent, FeedError> {
    if line.len() > FEED_LINE_BYTES_MAX {
        return Err(FeedError::LineTooLong);
    }
    let head: WireHead = serde_json::from_str(line)
        .map_err(|error| FeedError::Parse(format!("a line that is not an event: {error}")))?;
    let cursor = parse_number(&head.cursor, "cursor").map(Cursor)?;
    let kind = match head.kind.as_str() {
        "task.created" => EventKind::TaskCreated,
        "labels.changed" => EventKind::LabelsChanged,
        "task.claimed" => EventKind::TaskClaimed,
        "task.released" => EventKind::TaskReleased,
        "task.expired" => EventKind::TaskExpired,
        "task.blocked" => EventKind::TaskBlocked,
        "task.unblocked" => EventKind::TaskUnblocked,
        "task.ready" => EventKind::TaskReady,
        "task.reopened" => EventKind::TaskReopened,
        "task.completed" => EventKind::TaskCompleted,
        "task.failed" => EventKind::TaskFailed,
        "task.canceled" => EventKind::TaskCanceled,
        "message.posted" => EventKind::MessagePosted,
        _ => return Ok(FeedEvent::Other { cursor }),
    };
    let event: WireEvent = serde_json::from_str(line)
        .map_err(|error| field_error(cursor, &format!("not a {}: {error}", head.kind)))?;
    let WireEvent { message, actor } = event;
    if message.labels.len() > LABELS_MAX {
        return Err(FeedError::TooManyLabels);
    }
    let id = parse_number(&message.id, "message.id")?;
    // A reply's task is its thread's root; any other message is its own root.
    let task = match &message.re {
        None => id,
        Some(root) => parse_number(root, "message.re")?,
    };
    if kind.is_task_only() && task != id {
        return Err(field_error(cursor, "a task event on a reply"));
    }
    let actor = actor.map(|actor| actor.id);
    if actor.as_ref().is_some_and(String::is_empty) {
        return Err(field_error(cursor, "an empty actor.id"));
    }
    let known = BoardEvent {
        cursor,
        kind,
        task: TaskId(task),
        channel: message.channel,
        labels: message.labels.into_iter().collect(),
        state: message.task.map(|task| TaskState::from(task.state)),
        actor,
    };
    if kind.is_task_only() && known.state.is_none() {
        return Err(field_error(cursor, "a task event with no message.task"));
    }
    assert!(known.labels.len() <= LABELS_MAX);
    assert!(known.actor.as_ref().is_none_or(|actor| !actor.is_empty()));
    Ok(FeedEvent::Known(known))
}

fn field_error(cursor: Cursor, detail: &str) -> FeedError {
    assert!(!detail.is_empty());
    FeedError::Parse(format!("event {}: {detail}", cursor.0))
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
            ("task-ready", 21, TaskReady, 4, &[("board.task", "ready"), role], Ready, None),
            ("task-canceled", 22, TaskCanceled, 4, &canceled, Canceled, p),
        ];
        for (name, cursor, kind, task, labels, state, actor) in cases {
            let expected = known(cursor, kind, task, labels, Some(state), actor);
            assert_eq!(fixture(&format!("tail-{name}.json")), expected, "{name}");
        }
        let reply = known(14, MessagePosted, 2, &[], None, p);
        assert_eq!(fixture("tail-message-posted-reply.json"), reply);
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
        reader_with(name, body, read_lines)
    }

    fn reader_with(name: &str, body: &str, read: ReadLines) -> (FeedReader, PathBuf) {
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
        (FeedReader::start_with(&cli, Cursor(0), read).unwrap(), dir)
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
        let ready = fixture("tail-task-ready.json");
        assert_eq!(feed.next_item().unwrap(), FeedItem::Event(ready));
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
    fn reports_the_exit_while_a_descendant_keeps_printing() {
        const CALLS_MAX: u32 = 10_000_000;
        let unique = format!("rider-feed-chatty-pid-{}", std::process::id());
        let pid_file = std::env::temp_dir().join(unique);
        let body = format!(
            "while :; do echo '{}'; done &\necho $! > '{}'\nexit 3\n",
            r#"{"cursor":"8","type":"future.event"}"#,
            pid_file.display(),
        );
        let (mut feed, dir) = reader("chatty", &body);
        let started = Instant::now();
        let mut exits = Vec::new();
        for _ in 0..CALLS_MAX {
            match feed.next_item() {
                Ok(FeedItem::Event(_)) => {
                    let elapsed = started.elapsed();
                    assert!(elapsed < Duration::from_secs(10), "no exit in {elapsed:?}");
                }
                Err(FeedError::Ended) => break,
                item => exits.push(item.unwrap()),
            }
        }
        let elapsed = started.elapsed();
        drop(feed);
        let pid = fs::read_to_string(&pid_file).unwrap();
        fs::remove_file(&pid_file).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(exits, [FeedItem::Exited { code: Some(3) }]);
        assert!(
            elapsed < END_GRACE + Duration::from_secs(2),
            "took {elapsed:?}"
        );
        assert!(
            gone(pid.trim()),
            "the descendant {} outlived the feed",
            pid.trim()
        );
    }

    /// Items until `Ended`, the first error included.
    fn drain(feed: &mut FeedReader) -> Vec<Result<FeedItem, String>> {
        const ITEMS_MAX: usize = 1000;
        let mut items = Vec::new();
        for _ in 0..ITEMS_MAX {
            match feed.next_item() {
                Err(FeedError::Ended) => return items,
                Ok(item) => items.push(Ok(item)),
                Err(error) => items.push(Err(error.to_string())),
            }
        }
        panic!("the feed never ended: {items:?}");
    }

    fn other(cursor: u64) -> Result<FeedItem, String> {
        Ok(FeedItem::Event(FeedEvent::Other {
            cursor: Cursor(cursor),
        }))
    }

    #[test]
    fn delivers_every_line_to_a_slow_reader_then_the_exit() {
        let body = r#"printf '{"cursor":"%s","type":"x"}\n' 1 2 3
exit 3
"#;
        let (mut feed, dir) = reader("slow", body);
        thread::sleep(Duration::from_millis(400));
        let first = feed.next_item().map_err(|error| error.to_string());
        thread::sleep(END_GRACE + Duration::from_millis(200));
        let rest = drain(&mut feed);
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(first, other(1));
        let exited = Ok(FeedItem::Exited { code: Some(3) });
        assert_eq!(rest, [other(2), other(3), exited]);
    }

    #[test]
    fn reports_the_exit_when_only_the_exited_child_is_left() {
        let body = r#"printf '{"cursor":"%s","type":"x"}\n' 1 2
exit 4
"#;
        let (mut feed, dir) = reader("zombie", body);
        thread::sleep(Duration::from_millis(400));
        let first = feed.next_item().map_err(|error| error.to_string());
        thread::sleep(END_GRACE + Duration::from_millis(200));
        let second = feed.next_item().map_err(|error| error.to_string());
        let rest = drain(&mut feed);
        fs::remove_dir_all(dir).unwrap();
        assert_eq!([first, second], [other(1), other(2)]);
        assert_eq!(rest, [Ok(FeedItem::Exited { code: Some(4) })]);
    }

    /// Whether the process `pid` is gone within five seconds.
    fn gone(pid: &str) -> bool {
        let alive = || {
            Command::new("kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !alive()
        })
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
        let ready = fixture("tail-task-ready.json");
        assert_eq!(feed.next_item().unwrap(), FeedItem::Event(ready));
        drop(feed);
        let pid = fs::read_to_string(&pid_file).unwrap();
        fs::remove_file(&pid_file).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert!(
            gone(pid.trim()),
            "the descendant {} outlived the feed",
            pid.trim()
        );
    }

    #[test]
    fn rejects_an_over_long_string() {
        let result = parse_event(&" ".repeat(FEED_LINE_BYTES_MAX + 1));
        assert!(matches!(result, Err(FeedError::LineTooLong)), "{result:?}");
    }

    /// Starts reading only after `vb` has exited and its group has been killed.
    fn read_lines_late(stdout: BufReader<ChildStdout>, sender: &SyncSender<Received>) {
        thread::sleep(END_GRACE + Duration::from_millis(500));
        read_lines(stdout, sender);
    }

    #[test]
    fn delivers_every_line_when_the_reader_lags_the_exit() {
        let body = r#"printf '{"cursor":"%s","type":"x"}\n' 1 2 3
exit 3
"#;
        let (mut feed, dir) = reader_with("late", body, read_lines_late);
        let items = drain(&mut feed);
        fs::remove_dir_all(dir).unwrap();
        let exited = Ok(FeedItem::Exited { code: Some(3) });
        assert_eq!(items, [other(1), other(2), other(3), exited]);
    }
}
