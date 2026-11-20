//! The daemon's control socket: newline-delimited JSON requests in, replies and streams out.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::board::{TaskId, TaskState};
use crate::core::{
    AgentRow, BoardHealth, CLIENTS_MAX, ClientId, ErrorCode, Event, Phase, Reply, Request,
    StatusView, Stopping, TaskRow, Untracked,
};

/// The socket's file name under the Rider home.
pub const SOCKET_FILE: &str = "rider.sock";
/// The protocol version a request names in `v`.
pub const VERSION: u64 = 1;
/// Upper bound on one request line; a longer one is refused and the connection closed.
pub const REQUEST_BYTES_MAX: usize = 64 * 1024;
/// Upper bound on the lines waiting for one client; past it the client is dropped as `lagged`.
pub const CLIENT_BACKLOG_MAX: usize = 256;
/// Upper bound on one line a client reads back.
pub const REPLY_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Upper bound on the log files looked at for one agent's last output.
const LOG_FILES_MAX: usize = 64;
/// How long closing waits for queued lines to reach their clients.
const CLOSE_MILLIS_MAX: u64 = 2000;
const CLOSE_POLL_MILLIS: u64 = 20;
const ACCEPTS_MAX: u64 = u64::MAX;
const REQUESTS_MAX: u64 = u64::MAX;
const STREAM_LINES_MAX: u64 = u64::MAX;
/// Upper bound on one log line streamed to a client; a longer one is split.
const LOG_LINE_BYTES_MAX: usize = 1024 * 1024;
/// Upper bound on the log bytes read in one go.
const LOG_READ_BYTES_MAX: u64 = 4 * 1024 * 1024;
/// How often a followed log is read again.
const LOG_POLL_MILLIS: u64 = 100;
const LOG_POLLS_MAX: u64 = u64::MAX;
/// Upper bound on writer threads kept for joining at close; finished ones are dropped first.
const WRITERS_MAX: usize = 4 * CLIENTS_MAX;

/// Hands a socket event to the core; false once the core has gone.
pub type Sink = Arc<dyn Fn(Event) -> bool + Send + Sync>;

/// A request as it arrives on the wire.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    v: u64,
    id: u64,
    cmd: String,
    #[serde(default)]
    now: Option<bool>,
    #[serde(default)]
    task: Option<u64>,
    #[serde(default)]
    follow: Option<bool>,
}

struct Outgoing {
    id: u64,
    reply: Reply,
}

struct Slot {
    id: ClientId,
    /// The id of the request being answered, while one is.
    request: Option<u64>,
    lines: SyncSender<Outgoing>,
    stream: UnixStream,
}

#[derive(Default)]
struct Clients {
    slots: Vec<Slot>,
    writers: Vec<JoinHandle<()>>,
    next: u64,
    closed: bool,
}

/// The listening socket and its clients.
pub struct Socket {
    path: PathBuf,
    clients: Arc<Mutex<Clients>>,
}

fn lock(clients: &Mutex<Clients>) -> MutexGuard<'_, Clients> {
    // Every holder leaves the clients whole between its statements.
    clients.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Binds `path` with mode 0600, replacing a socket left there, and serves it on its own threads.
pub fn listen(path: &Path, logs: PathBuf, sink: Sink) -> io::Result<Socket> {
    assert!(path.is_absolute() && logs.is_absolute());
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "it exists and is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let clients = Arc::new(Mutex::new(Clients::default()));
    let shared = Arc::clone(&clients);
    thread::Builder::new()
        .name("socket".into())
        .spawn(move || accept(&listener, &shared, &logs, &sink))?;
    assert!(path.exists());
    Ok(Socket {
        path: path.to_path_buf(),
        clients,
    })
}

fn accept(listener: &UnixListener, clients: &Arc<Mutex<Clients>>, logs: &Path, sink: &Sink) {
    for _ in 0..ACCEPTS_MAX {
        let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) else {
            continue;
        };
        let mut guard = lock(clients);
        if guard.closed {
            return;
        }
        if guard.slots.len() >= CLIENTS_MAX {
            drop(guard);
            let line = error_line(None, "busy", "too many clients are connected");
            let _ = stream.write_all(line.as_bytes());
            continue;
        }
        let (Ok(reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
            continue;
        };
        guard.next = guard.next.saturating_add(1);
        let id = ClientId(guard.next);
        let (lines, queued) = sync_channel::<Outgoing>(CLIENT_BACKLOG_MAX);
        let logs = logs.to_path_buf();
        let spawned = thread::Builder::new()
            .name("client-writer".into())
            .spawn(move || write_replies(writer, &queued, &logs));
        let Ok(handle) = spawned else { continue };
        guard.writers.retain(|writer| !writer.is_finished());
        if guard.writers.len() < WRITERS_MAX {
            guard.writers.push(handle);
        }
        guard.slots.push(Slot {
            id,
            request: None,
            lines,
            stream,
        });
        assert!(guard.slots.len() <= CLIENTS_MAX);
        drop(guard);
        let (held, sink) = (Arc::clone(clients), Arc::clone(sink));
        let spawned = thread::Builder::new()
            .name("client-reader".into())
            .spawn(move || read_requests(reader, id, &held, &sink));
        if spawned.is_err() {
            lock(clients).slots.retain(|slot| slot.id != id);
        }
    }
}

// Reads requests until the client goes, then tells the core.
fn read_requests(stream: UnixStream, id: ClientId, clients: &Mutex<Clients>, sink: &Sink) {
    let mut reader = BufReader::new(stream);
    for _ in 0..REQUESTS_MAX {
        let mut bytes = Vec::new();
        let limit = u64::try_from(REQUEST_BYTES_MAX).unwrap_or(u64::MAX);
        let read = (&mut reader).take(limit + 1).read_until(b'\n', &mut bytes);
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) if bytes.len() > REQUEST_BYTES_MAX => {
                let message = format!("a request is at most {REQUEST_BYTES_MAX} bytes");
                refuse(clients, id, None, &message);
                break;
            }
            Ok(_) => {}
        }
        match parse_request(&bytes) {
            Ok((request_id, request)) => {
                let mut guard = lock(clients);
                let Some(slot) = guard.slots.iter_mut().find(|slot| slot.id == id) else {
                    break;
                };
                if slot.request.is_some() {
                    drop(guard);
                    let message = "one command at a time per connection";
                    refuse(clients, id, Some(request_id), message);
                    continue;
                }
                slot.request = Some(request_id);
                drop(guard);
                if !sink(Event::Client {
                    client: id,
                    request,
                }) {
                    break;
                }
            }
            Err((request_id, message)) => refuse(clients, id, request_id, &message),
        }
    }
    lock(clients).slots.retain(|slot| slot.id != id);
    let _ = sink(Event::ClientGone(id));
}

fn refuse(clients: &Mutex<Clients>, id: ClientId, request: Option<u64>, message: &str) {
    let guard = lock(clients);
    if let Some(slot) = guard.slots.iter().find(|slot| slot.id == id) {
        let line = error_line(request, "bad_request", message);
        // A client too slow to take its refusal is dropped by the next reply instead.
        let _ = (&slot.stream).write_all(line.as_bytes());
    }
}

/// The request in `bytes`, or the request id, if readable, and why it is refused.
fn parse_request(bytes: &[u8]) -> Result<(u64, Request), (Option<u64>, String)> {
    let wire: WireRequest = serde_json::from_slice(bytes).map_err(|error| {
        let id = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_u64));
        (id, format!("malformed request: {error}"))
    })?;
    if wire.v != VERSION {
        return Err((Some(wire.id), format!("unknown version {}", wire.v)));
    }
    let fields = (wire.now, wire.task.map(TaskId), wire.follow);
    let request = match (wire.cmd.as_str(), fields) {
        ("status", (None, None, None)) => Request::Status,
        ("agents", (None, None, None)) => Request::Agents,
        ("down", (now, None, None)) => Request::Down {
            now: now.unwrap_or(false),
        },
        ("kill", (None, Some(task), None)) => Request::Kill(task),
        ("logs", (None, Some(task), follow)) => Request::Logs {
            task,
            follow: follow.unwrap_or(false),
        },
        (cmd @ ("status" | "agents" | "down" | "kill" | "logs"), _) => {
            return Err((Some(wire.id), format!("wrong fields for {cmd:?}")));
        }
        (cmd, _) => return Err((Some(wire.id), format!("unknown command {cmd:?}"))),
    };
    Ok((wire.id, request))
}

impl Socket {
    /// Queues `reply` for `client`; a client whose backlog is full is dropped as `lagged`.
    pub fn reply(&self, client: ClientId, reply: Reply) {
        let mut guard = lock(&self.clients);
        let Some(index) = guard.slots.iter().position(|slot| slot.id == client) else {
            return;
        };
        let slot = &mut guard.slots[index];
        let Some(id) = slot.request else { return };
        let last = match reply {
            Reply::Status(_) | Reply::Agents(_) | Reply::End | Reply::Done => true,
            Reply::Refused { .. } | Reply::Log { follow: false, .. } => true,
            Reply::Accepted | Reply::Line(_) | Reply::Log { follow: true, .. } => false,
        };
        if last {
            slot.request = None;
        }
        match slot.lines.try_send(Outgoing { id, reply }) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                let slot = guard.slots.remove(index);
                let line = error_line(Some(id), "lagged", "the client read too slowly");
                // The writer is stuck on this client, so the refusal may not get through.
                let _ = slot.stream.set_nonblocking(true);
                let _ = (&slot.stream).write_all(line.as_bytes());
                let _ = slot.stream.shutdown(Shutdown::Both);
            }
        }
        assert!(guard.slots.len() <= CLIENTS_MAX);
    }

    /// Stops accepting, lets queued lines reach their clients for a bounded time, and removes
    /// the socket file.
    pub fn close(&self) {
        let writers = {
            let mut guard = lock(&self.clients);
            guard.closed = true;
            for slot in guard.slots.drain(..) {
                let _ = slot.stream.shutdown(Shutdown::Read);
            }
            std::mem::take(&mut guard.writers)
        };
        // Wakes the acceptor so it sees `closed`.
        let _ = UnixStream::connect(&self.path);
        let _ = fs::remove_file(&self.path);
        let deadline = Instant::now() + Duration::from_millis(CLOSE_MILLIS_MAX);
        for writer in writers {
            while !writer.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(CLOSE_POLL_MILLIS));
            }
            if writer.is_finished() {
                let _ = writer.join();
            }
        }
    }
}

fn write_replies(mut stream: UnixStream, queued: &Receiver<Outgoing>, logs: &Path) {
    for _ in 0..STREAM_LINES_MAX {
        let Ok(outgoing) = queued.recv() else { break };
        let written = match outgoing.reply {
            Reply::Log { task, follow } => {
                stream_log(&mut stream, outgoing.id, task, follow, queued, logs)
            }
            reply => {
                let line = reply_line(outgoing.id, &reply, logs);
                stream.write_all(line.as_bytes()).is_ok()
            }
        };
        if !written {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Write);
}

/// The newest of the task's attempt logs (`*.jsonl`) under `logs`, if it has any.
pub fn newest_log(logs: &Path, task: TaskId) -> Option<PathBuf> {
    let entries = fs::read_dir(logs.join(task.0.to_string())).ok()?;
    let newest = entries
        .take(LOG_FILES_MAX)
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            path.extension().filter(|extension| *extension == "jsonl")?;
            let metadata = entry.metadata().ok().filter(fs::Metadata::is_file)?;
            Some((metadata.modified().ok()?, path))
        })
        .max()?;
    assert!(newest.1.starts_with(logs));
    Some(newest.1)
}

// Streams the task's newest log; with `follow`, until the core ends the stream. False once
// the client or the socket is gone.
fn stream_log(
    stream: &mut UnixStream,
    id: u64,
    task: TaskId,
    follow: bool,
    queued: &Receiver<Outgoing>,
    logs: &Path,
) -> bool {
    let mut file: Option<File> = None;
    let mut pending: Vec<u8> = Vec::new();
    let mut ended = !follow;
    for _ in 0..LOG_POLLS_MAX {
        if file.is_none() {
            file = newest_log(logs, task).and_then(|path| File::open(path).ok());
            let ok = format!("{}\n", json!({"id": id, "ok": true}));
            if file.is_some() && stream.write_all(ok.as_bytes()).is_err() {
                return false;
            }
        }
        let mut full = false;
        if let Some(file) = file.as_mut() {
            let read = file.take(LOG_READ_BYTES_MAX).read_to_end(&mut pending);
            full = read.is_ok_and(|read| read as u64 == LOG_READ_BYTES_MAX);
            if !write_log_lines(stream, id, &mut pending, false) {
                return false;
            }
        }
        if ended && !full {
            let last = match file {
                None => {
                    let message = format!("task #{} has no log", task.0);
                    error_line(Some(id), "unknown_task", &message)
                }
                Some(_) if !write_log_lines(stream, id, &mut pending, true) => return false,
                Some(_) => format!("{}\n", json!({"id": id, "end": true})),
            };
            return stream.write_all(last.as_bytes()).is_ok();
        }
        if !ended {
            match queued.recv_timeout(Duration::from_millis(LOG_POLL_MILLIS)) {
                Ok(Outgoing {
                    reply: Reply::End, ..
                }) => ended = true,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }
    false
}

// Writes each whole line in `pending` as a stream line, and the rest too when `all`; a line
// longer than `LOG_LINE_BYTES_MAX` is split.
fn write_log_lines(stream: &mut UnixStream, id: u64, pending: &mut Vec<u8>, all: bool) -> bool {
    let mut start = 0;
    let mut lines = String::new();
    while start < pending.len() {
        let rest = &pending[start..];
        let (line, used) = match rest.iter().position(|byte| *byte == b'\n') {
            Some(end) if end <= LOG_LINE_BYTES_MAX => (&rest[..end], end + 1),
            _ if rest.len() > LOG_LINE_BYTES_MAX => {
                (&rest[..LOG_LINE_BYTES_MAX], LOG_LINE_BYTES_MAX)
            }
            _ if all => (rest, rest.len()),
            _ => break,
        };
        let text = String::from_utf8_lossy(line);
        lines.push_str(&format!("{}\n", json!({"id": id, "stream": text})));
        start += used;
    }
    assert!(start <= pending.len());
    pending.drain(..start);
    assert!(!all || pending.is_empty());
    stream.write_all(lines.as_bytes()).is_ok()
}

fn error_line(id: Option<u64>, code: &str, message: &str) -> String {
    let value = json!({"id": id, "ok": false, "error": {"code": code, "message": message}});
    format!("{value}\n")
}

fn reply_line(id: u64, reply: &Reply, logs: &Path) -> String {
    let value = match reply {
        Reply::Status(view) => json!({"id": id, "ok": true, "data": status_json(view)}),
        Reply::Agents(rows) => {
            let rows: Vec<Value> = rows.iter().map(|row| agent_json(row, logs)).collect();
            json!({"id": id, "ok": true, "data": rows})
        }
        Reply::Accepted | Reply::Done | Reply::Log { .. } => json!({"id": id, "ok": true}),
        Reply::Refused { code, message } => {
            let code = match code {
                ErrorCode::UnknownTask => "unknown_task",
                ErrorCode::NotRunning => "not_running",
            };
            json!({"id": id, "ok": false, "error": {"code": code, "message": message}})
        }
        Reply::Line(text) => json!({"id": id, "stream": text}),
        Reply::End => json!({"id": id, "end": true}),
    };
    format!("{value}\n")
}

fn status_json(view: &StatusView) -> Value {
    let board = match view.board {
        BoardHealth::Up => "up",
        BoardHealth::Down { .. } => "down",
    };
    let stopping = match view.stopping {
        Stopping::No => "no",
        Stopping::Drain { .. } => "drain",
        Stopping::Now => "now",
    };
    let (untracked, more) = match view.untracked {
        Untracked::Exactly(count) => (count, false),
        Untracked::AtLeast(count) => (count, true),
    };
    let tasks: Vec<Value> = view.tasks.iter().map(task_json).collect();
    json!({
        "board": board,
        "stopping": stopping,
        "running": view.running,
        "limit": view.limit,
        "untracked": untracked,
        "untracked_at_least": more,
        "tasks": tasks,
    })
}

fn task_json(row: &TaskRow) -> Value {
    json!({
        "id": row.id.0,
        "project": row.project,
        "state": state_name(row.state),
        "role": row.role,
        "profile": row.profile,
        "age_ms": row.age_millis,
        "branch": row.branch,
    })
}

fn agent_json(row: &AgentRow, logs: &Path) -> Value {
    json!({
        "task": row.task.0,
        "role": row.role,
        "profile": row.profile,
        "phase": phase_name(row.phase),
        "age_ms": row.age_millis,
        "output_ms_ago": output_millis_ago(logs, row.task),
    })
}

// How long ago the newest of the task's logs was written, if any was.
fn output_millis_ago(logs: &Path, task: TaskId) -> Option<u64> {
    let entries = fs::read_dir(logs.join(task.0.to_string())).ok()?;
    let newest = entries
        .take(LOG_FILES_MAX)
        .filter_map(|entry| entry.ok()?.metadata().ok()?.modified().ok())
        .max()?;
    let ago = SystemTime::now().duration_since(newest).unwrap_or_default();
    Some(u64::try_from(ago.as_millis()).unwrap_or(u64::MAX))
}

/// A task state's board name.
pub fn state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Waiting => "waiting",
        TaskState::Ready => "ready",
        TaskState::Claimed => "claimed",
        TaskState::Blocked => "blocked",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Claiming => "claiming",
        Phase::Preparing => "preparing",
        Phase::Running => "running",
        Phase::Judging => "judging",
        Phase::Settling => "settling",
        Phase::Stopping(_) => "stopping",
        Phase::Abandoning => "abandoning",
    }
}

/// Why a client could not talk to the daemon.
#[derive(Debug)]
pub enum ClientError {
    /// No daemon listens on the socket.
    NotRunning(PathBuf),
    Io(String),
    /// The daemon answered with something that is not a reply.
    Malformed(String),
    /// The daemon refused the request.
    Refused {
        code: String,
        message: String,
    },
    /// The connection closed before the reply ended.
    Closed,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotRunning(path) => {
                write!(
                    f,
                    "the daemon is not running: nothing answers on {}",
                    path.display()
                )
            }
            ClientError::Io(message) => write!(f, "talking to the daemon: {message}"),
            ClientError::Malformed(line) => write!(f, "the daemon answered {line:?}"),
            ClientError::Refused { code, message } => write!(f, "{code}: {message}"),
            ClientError::Closed => write!(f, "the daemon closed the connection"),
        }
    }
}

/// One connection to the daemon, sending one command.
pub struct Client {
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connects to the socket under `home`.
    pub fn connect(home: &Path) -> Result<Client, ClientError> {
        let path = home.join(SOCKET_FILE);
        let stream = UnixStream::connect(&path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning(path.clone())
            }
            _ => ClientError::Io(error.to_string()),
        })?;
        Ok(Client {
            reader: BufReader::new(stream),
        })
    }

    /// Sends `cmd` with its `fields`, and returns the `ok` reply's data.
    pub fn send(&mut self, cmd: &str, fields: &[(&str, Value)]) -> Result<Value, ClientError> {
        assert!(!cmd.is_empty());
        let mut request = json!({"v": VERSION, "id": 1, "cmd": cmd});
        for (name, value) in fields {
            assert!(!matches!(*name, "v" | "id" | "cmd"));
            request[*name] = value.clone();
        }
        let line = format!("{request}\n");
        assert!(line.len() <= REQUEST_BYTES_MAX);
        let io = |error: io::Error| ClientError::Io(error.to_string());
        self.reader
            .get_mut()
            .write_all(line.as_bytes())
            .map_err(io)?;
        let mut reply = self.next()?;
        if reply.get("ok") != Some(&Value::Bool(true)) {
            return Err(refusal(&reply));
        }
        Ok(reply
            .get_mut("data")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    /// The next line of a stream, or `None` at its end.
    pub fn next_line(&mut self) -> Result<Option<String>, ClientError> {
        let reply = self.next()?;
        if reply.get("end") == Some(&Value::Bool(true)) {
            return Ok(None);
        }
        match reply.get("stream").and_then(Value::as_str) {
            Some(text) => Ok(Some(text.to_string())),
            None => Err(refusal(&reply)),
        }
    }

    fn next(&mut self) -> Result<Value, ClientError> {
        let mut bytes = Vec::new();
        let limit = u64::try_from(REPLY_BYTES_MAX).unwrap_or(u64::MAX);
        let read = (&mut self.reader).take(limit).read_until(b'\n', &mut bytes);
        let read = read.map_err(|error| ClientError::Io(error.to_string()))?;
        if read == 0 || !bytes.ends_with(b"\n") {
            return Err(ClientError::Closed);
        }
        let text = String::from_utf8_lossy(&bytes);
        serde_json::from_slice(&bytes).map_err(|_| ClientError::Malformed(text.trim().to_string()))
    }
}

fn refusal(reply: &Value) -> ClientError {
    let error = reply.get("error");
    let field = |name: &str| {
        let value = error
            .and_then(|error| error.get(name))
            .and_then(Value::as_str);
        value.map(str::to_string)
    };
    match (field("code"), field("message")) {
        (Some(code), Some(message)) => ClientError::Refused { code, message },
        _ => ClientError::Malformed(reply.to_string()),
    }
}
