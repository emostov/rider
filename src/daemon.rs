//! `rider daemon`: the core driven by the board's feed, a timer, signals and worker threads.

use std::cell::Cell;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::{FileExt, TryLockError};
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use serde::{Deserialize, Serialize};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::board::{
    Board, BoardError, Listing, TaskId, TaskState, TaskView, Thread, Timestamp, VbCli, one_line,
    parse_failure_reason, register_schemas,
};
use crate::cli::board_serve;
use crate::config::Limits;
use crate::core::{
    self, ACTIONS_MAX, ATTEMPT_MILLIS_MAX, Action, AgentRecord, Call, Config, Event, LEASE_MILLIS,
    LEFTOVERS_MAX, PidFile, RunId, Settlement, State, Stopping, Unaccounted,
};
use crate::feed::{Cursor, FeedError, FeedEvent, FeedItem, FeedReader, FeedStop};
use crate::harness::{ATTEMPT_SECONDS_MAX, CommandLine, Exit, HarnessError, KILL_GRACE_SECONDS};
use crate::reap;
use crate::roles::{AGENTS_MAX, RoleSpec};
use crate::run_task::{
    Attempt, Env, IdentityUnknown, Judgement, Prepared, RunError, board_identity_at_start,
    judge_attempt, launch_agent, prepare_attempt, settle_attempt,
};
use crate::settle::Verdict;
use crate::socket::{self, SOCKET_FILE, Socket};

/// Upper bound on the events waiting for the core; sources block when it is full.
pub const EVENT_QUEUE_MAX: usize = 1024;
/// The timer's period.
pub const TICK_SECONDS: u64 = 5;
/// How often the core takes a full listing of open tasks.
pub const RECONCILE_SECONDS: u64 = 60;
/// The worker threads that carry out actions; an agent holds one for as long as it runs.
pub const WORKERS_MAX: usize = 2 * AGENTS_MAX as usize + 4;
/// Upper bound on the actions waiting for a worker.
pub const JOBS_QUEUED_MAX: usize = 4 * ACTIONS_MAX;
/// The channel subtree the daemon works.
pub const SUBTREE: &str = "work/";
/// The profile the daemon lists and follows the board as.
pub const DAEMON_PROFILE: &str = "rider-daemon";
/// What the daemon prints once it follows the board.
pub const FOLLOWING: &str = "rider daemon: following";
const DRAIN_LINE: &str = "a stop signal: draining, claiming nothing new";
const STOP_NOW_LINE: &str = "a second stop signal: stopping now";
/// The test-only setting that shortens limits: `lease=<ms>,attempt=<ms>,tick=<ms>,grace=<ms>`,
/// any subset.
pub const TEST_MILLIS_VAR: &str = "RIDER_TEST_MILLIS";
const TEST_SETTINGS_MAX: usize = 4;
const GRACE_MILLIS: u64 = KILL_GRACE_SECONDS * 1000;
const STOP_POLL_MILLIS: u64 = 50;
/// Upper bound on the agents one attempt starts: a resumed session and its fresh start.
const AGENT_GROUPS_MAX: usize = 2;
/// How long the daemon waits on exit for the feed thread once its group is killed: past the
/// feed's own grace for its stdout to close after a kill.
const FEED_JOIN_MILLIS_MAX: u64 = 15_000;
const _: () = assert!(FEED_JOIN_MILLIS_MAX >= STOP_POLL_MILLIS);
const _: () = assert!(WORKERS_MAX > AGENTS_MAX as usize && TICK_SECONDS > 0);
const NO_CHANNEL: &str = "channel_not_found";
const LOCK_FILE: &str = "daemon.lock";
/// The daemon's log under the Rider home.
pub const LOG_FILE: &str = "daemon.log";
/// The output of the board the daemon started, under the Rider home.
pub const BOARD_LOG_FILE: &str = "board.log";
/// The daemon log's line naming the board process the daemon started.
pub const BOARD_STARTED: &str = "started the board, pid";
/// The daemon log's line naming the board process a crashed daemon started, kept as its own.
pub const BOARD_KEPT: &str = "kept the board started before, pid";
/// The file under the Rider home naming the board process the daemon started and its start.
pub const BOARD_FILE: &str = "board.json";
const BOARD_FILE_BYTES_MAX: u64 = 256;
/// How often the daemon checks that a board it kept from before still runs.
const BOARD_POLL_MILLIS: u64 = 200;
const BOARD_POLLS_MAX: u64 = u64::MAX;
/// How long a board the daemon started is given to end after SIGTERM before SIGKILL.
const BOARD_STOP_MILLIS_MAX: u64 = 10_000;
const AGENTS_DIR: &str = "agents";
const STAGED_SUFFIX: &str = ".new";
const LEFT_EXTENSION: &str = "left";
const STOP_EXTENSION: &str = "stop";
/// The directory under the Rider home holding each task's attempt logs.
pub const LOGS_DIR: &str = "logs";
const LOCK_BYTES_MAX: u64 = 256;
const PID_BYTES_MAX: u64 = 32;
const STEPS_MAX: u64 = u64::MAX;
const TICKS_MAX: u64 = u64::MAX;
const SIGNALS_MAX: usize = 1 << 20;
const FEED_ITEMS_MAX: u64 = 1 << 40;
const JOBS_MAX: u64 = u64::MAX;
/// Upper bound on the log lines waiting for the log thread; past it lines are dropped.
const LOG_QUEUE_MAX: usize = 256;
const LOG_LINES_MAX: u64 = u64::MAX;
/// Why a settlement's board write was refused: the daemon is stopping now.
const STOPPED: &str = "the daemon is stopping now";
/// The file under the Rider home naming each running agent's process group and start time.
pub const AGENTS_FILE: &str = "agents.json";
/// The file under the Rider home holding the last feed cursor the core handled.
pub const CURSOR_FILE: &str = "cursor";
const AGENTS_BYTES_MAX: u64 = 64 * 1024;
const CURSOR_BYTES_MAX: u64 = 32;
/// Upper bound on the agents `agents.json` names: every group of every attempt.
const AGENT_RECORDS_MAX: usize = AGENTS_MAX as usize * AGENT_GROUPS_MAX;
/// Upper bound on the entries read from the agents directory.
const AGENT_FILES_MAX: usize = 4 * AGENT_RECORDS_MAX;
const _: () = assert!(AGENT_FILES_MAX <= LEFTOVERS_MAX);
const ACK_BYTES_MAX: u64 = 64;
/// Upper bound on the agent starts and ends waiting for the recorder.
const RECORDER_QUEUE_MAX: usize = 4 * AGENTS_MAX as usize;
/// How often the recorder looks for a pid file that has appeared or changed.
const RECORD_POLL_MILLIS: u64 = 100;
const RECORDER_ROUNDS_MAX: u64 = u64::MAX;
// Writes its own pid, which is its process group, then becomes the agent only once the daemon
// has recorded that group in `agents.json` and acknowledged it, naming the pid, in the ack file.
// Unacknowledged after about 30 s, it exits without starting the agent.
const PID_WRAPPER: &str = r#"echo $$ > "$RIDER_AGENT_PID_FILE" || exit 1
n=0
until read -r group start 2>/dev/null < "$RIDER_AGENT_ACK_FILE" && [ "$group" = "$$" ]; do
  n=$((n + 1))
  [ "$n" -gt 600 ] && exit 1
  sleep 0.05
done
[ -e "$RIDER_AGENT_STOP_FILE" ] && exit 1
PATH="$RIDER_AGENT_VB_DIR${PATH:+:$PATH}"
export PATH
exec "$@""#;

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "macos")]
const SANDBOX_REFUSED_EXIT: i32 = 71;
#[cfg(target_os = "macos")]
const SANDBOX_PROBE: &str =
    "(version 1)\n(allow default)\n(deny file-write* (literal \"/var/empty/rider-probe\"))\n";
#[cfg(target_os = "macos")]
const HOME_DEPTH_MAX: usize = 64;
#[cfg(target_os = "macos")]
const CONTROL_FILES: [&str; 7] = [
    SOCKET_FILE,
    LOCK_FILE,
    AGENTS_FILE,
    CURSOR_FILE,
    BOARD_FILE,
    LOG_FILE,
    BOARD_LOG_FILE,
];

/// Where the daemon's files are and how it reaches the board and its agents.
pub const NODE_DIR: &str = "node";

pub struct Paths {
    pub home: PathBuf,
    pub vb: PathBuf,
    pub board_url: String,
}

/// Why the daemon could not start or keep going.
#[derive(Debug)]
pub enum DaemonError {
    /// Another process holds the lock.
    Held {
        path: PathBuf,
        holder: String,
    },
    Io {
        action: &'static str,
        path: PathBuf,
        message: String,
    },
    Thread(String),
    /// More actions waited for a worker than `JOBS_QUEUED_MAX`.
    JobsFull,
    /// Every source of events has gone.
    QueueClosed,
    /// `TEST_MILLIS_VAR` is malformed.
    Setting(String),
    /// The daemon's log exists and is not a regular file.
    NotAFile(PathBuf),
    /// The board's identity could not be read at start-up.
    Identity(IdentityUnknown),
    Schemas(BoardError),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::Held { path, holder } => {
                write!(f, "{} is held by {holder}", path.display())
            }
            DaemonError::Io {
                action,
                path,
                message,
            } => write!(f, "could not {action} {}: {message}", path.display()),
            DaemonError::Thread(message) => write!(f, "could not start a thread: {message}"),
            DaemonError::JobsFull => write!(f, "more than {JOBS_QUEUED_MAX} actions waited"),
            DaemonError::QueueClosed => write!(f, "the daemon's event queue closed"),
            DaemonError::Setting(message) => write!(f, "{TEST_MILLIS_VAR}: {message}"),
            DaemonError::NotAFile(path) => write!(f, "{} is not a regular file", path.display()),
            DaemonError::Identity(error) => write!(f, "{error}"),
            DaemonError::Schemas(error) => {
                write!(
                    f,
                    "could not register Rider's schemas on the board: {error}"
                )
            }
        }
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> DaemonError {
    let path = path.to_path_buf();
    move |error| DaemonError::Io {
        action,
        path,
        message: error.to_string(),
    }
}

struct Timing {
    lease_millis: u64,
    attempt_millis_max: u64,
    tick_millis: u64,
    grace_millis: u64,
    runner_millis: u64,
}

impl Timing {
    fn parse(setting: Option<&str>) -> Result<Timing, DaemonError> {
        let mut timing = Timing {
            lease_millis: LEASE_MILLIS,
            attempt_millis_max: ATTEMPT_MILLIS_MAX,
            tick_millis: TICK_SECONDS * 1000,
            grace_millis: GRACE_MILLIS,
            runner_millis: ATTEMPT_SECONDS_MAX * 1000,
        };
        let pairs = setting
            .unwrap_or_default()
            .split(',')
            .filter(|pair| !pair.is_empty());
        for (index, pair) in pairs.enumerate() {
            if index >= TEST_SETTINGS_MAX {
                return Err(DaemonError::Setting(format!(
                    "more than {TEST_SETTINGS_MAX} settings"
                )));
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let (field, floor, ceiling) = match key {
                "lease" => (&mut timing.lease_millis, 3, LEASE_MILLIS),
                "attempt" => (&mut timing.attempt_millis_max, 1, ATTEMPT_MILLIS_MAX),
                "tick" => (&mut timing.tick_millis, 1, TICK_SECONDS * 1000),
                "grace" => (&mut timing.grace_millis, 1, GRACE_MILLIS),
                "runner" => (&mut timing.runner_millis, 1000, ATTEMPT_SECONDS_MAX * 1000),
                _ => return Err(DaemonError::Setting(format!("unknown setting {key:?}"))),
            };
            *field = value
                .parse::<u64>()
                .ok()
                .filter(|millis| (floor..=ceiling).contains(millis))
                .ok_or_else(|| {
                    DaemonError::Setting(format!(
                        "{key} must be milliseconds from {floor} to {ceiling}, not {value:?}"
                    ))
                })?;
        }
        assert!(timing.lease_millis >= 3 && timing.lease_millis <= LEASE_MILLIS);
        assert!(timing.tick_millis > 0 && timing.attempt_millis_max > 0);
        assert!(timing.grace_millis > 0 && timing.grace_millis <= GRACE_MILLIS);
        assert!(timing.runner_millis >= 1000);
        assert!(timing.runner_millis <= ATTEMPT_SECONDS_MAX * 1000);
        Ok(timing)
    }
}

/// The daemon lock under the Rider home, held until dropped or the process ends.
pub struct Lock {
    _file: File,
}

/// Takes `daemon.lock` under `home` with an exclusive OS lock and writes our pid into it.
pub fn take_lock(home: &Path) -> Result<Lock, DaemonError> {
    assert!(home.is_absolute());
    fs::create_dir_all(home).map_err(io_error("create", home))?;
    let path = home.join(LOCK_FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io_error("open", &path))?;
    match FileExt::try_lock(&file) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let read = (&file).take(LOCK_BYTES_MAX).read_to_string(&mut text);
            read.map_err(io_error("read", &path))?;
            let holder = match one_line(text.trim()) {
                line if line.is_empty() => "a process that has not written its pid".to_string(),
                line => line,
            };
            return Err(DaemonError::Held { path, holder });
        }
        Err(TryLockError::Error(error)) => return Err(io_error("lock", &path)(error)),
    }
    let text = format!(
        "pid {} started {}\n",
        std::process::id(),
        clock_line(SystemTime::now())
    );
    file.set_len(0).map_err(io_error("truncate", &path))?;
    file.write_all(text.as_bytes())
        .map_err(io_error("write", &path))?;
    assert!(path.starts_with(home));
    Ok(Lock { _file: file })
}

/// What a worker hands back with its result.
enum Extra {
    None,
    Attempt(Arc<Attempt>),
    Launched(Result<Exit, HarnessError>),
    Judgement(Arc<Judgement>),
}

struct Input {
    event: Event,
    extra: Extra,
}

impl Input {
    fn plain(event: Event) -> Input {
        Input {
            event,
            extra: Extra::None,
        }
    }
}

type Job = Box<dyn FnOnce() -> Input + Send>;

// What the core thread keeps per run, beside the core's own record of it.
struct RunContext {
    run: RunId,
    task: TaskId,
    profile: String,
    attempt: Option<Arc<Attempt>>,
    launched: Option<Result<Exit, HarnessError>>,
    judgement: Option<Arc<Judgement>>,
}

impl RunContext {
    fn new(run: RunId, task: TaskId, profile: String) -> RunContext {
        assert!(!profile.is_empty());
        let context = RunContext {
            run,
            task,
            profile,
            attempt: None,
            launched: None,
            judgement: None,
        };
        assert!(context.attempt.is_none() && context.judgement.is_none());
        context
    }
}

struct Shared {
    paths: Paths,
    events: SyncSender<Input>,
    lease_millis: u64,
    grace_millis: u64,
    runner_seconds: u64,
    confine: bool,
    attempts: u8,
    feed: Mutex<FeedSlot>,
    /// The board process the daemon started, if it had to.
    served: Mutex<ServedSlot>,
    socket: Socket,
    /// The core has decided to stop now; a settlement checks it before each board write.
    stop_now: AtomicBool,
    log: Log,
    /// Tells the recorder of each agent's start and end.
    recorder: SyncSender<Recorded>,
    /// The cursor saved before the daemon last stopped, if any.
    saved_cursor: Option<Cursor>,
}

/// The current feed thread and how to end it.
#[derive(Default)]
struct FeedSlot {
    /// The daemon is exiting: no feed starts and none reports its loss.
    stopping: bool,
    stop: Option<FeedStop>,
    thread: Option<JoinHandle<()>>,
}

/// The board process the daemon started and the thread waiting on it.
#[derive(Default)]
struct ServedSlot {
    /// The daemon is exiting: no board starts and none reports its exit.
    stopping: bool,
    group: Option<Pid>,
    thread: Option<JoinHandle<()>>,
}

impl Shared {
    fn board(&self, profile: &str) -> VbCli {
        assert!(!profile.is_empty());
        assert!(self.paths.vb.is_absolute());
        VbCli {
            vb: self.paths.vb.clone(),
            url: self.paths.board_url.clone(),
            profile: profile.to_string(),
        }
    }

    fn env(&self, profile: &str) -> Env {
        assert!(!profile.is_empty());
        assert!(self.paths.home.is_absolute());
        Env {
            home: self.paths.home.clone(),
            profile: profile.to_string(),
            board_url: self.paths.board_url.clone(),
            attempts: self.attempts,
        }
    }

    // The board's lease end, cut to the daemon's own lease from `sent`, so a shortened lease
    // renews at a third of itself while the board keeps its longer one.
    fn lease_until(&self, sent: Timestamp, board: Timestamp) -> Timestamp {
        assert!(self.lease_millis >= 3);
        let own = Timestamp(sent.0.saturating_add(self.lease_millis));
        let until = own.min(board);
        assert!(until <= board);
        until
    }

    fn feed(&self) -> MutexGuard<'_, FeedSlot> {
        // Every holder leaves the slot whole between its statements.
        self.feed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn served(&self) -> MutexGuard<'_, ServedSlot> {
        // Every holder leaves the slot whole between its statements.
        self.served.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pid_file(&self, task: TaskId) -> PathBuf {
        pid_file(&self.paths.home, task)
    }
}

fn pid_file(home: &Path, task: TaskId) -> PathBuf {
    let path = home.join(AGENTS_DIR).join(format!("{}.pid", task.0));
    assert!(path.starts_with(home));
    path
}

// The file acknowledging that the group a pid file names is recorded in `agents.json`.
fn ack_file(pid_file: &Path) -> PathBuf {
    let path = pid_file.with_extension("ack");
    assert!(path != pid_file);
    path
}

fn stop_file(pid_file: &Path) -> PathBuf {
    let path = pid_file.with_extension(STOP_EXTENSION);
    assert!(path != pid_file && path != ack_file(pid_file));
    path
}

/// Runs the daemon in the foreground until it is stopped; returns its exit code.
/// Runs the daemon on the role table `roles` under `limits` until it is told to stop.
pub fn run(paths: Paths, roles: &[RoleSpec], limits: Limits) -> Result<u8, DaemonError> {
    assert!(paths.home.is_absolute() && paths.vb.is_absolute());
    assert!(limits.agents > 0 && limits.agents <= AGENTS_MAX);
    let setting = std::env::var(TEST_MILLIS_VAR);
    let setting = match &setting {
        Ok(value) => Some(value.as_str()),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(DaemonError::Setting("not UTF-8".to_string()));
        }
    };
    let timing = Timing::parse(setting)?;
    let _lock = take_lock(&paths.home)?;
    let agents = paths.home.join(AGENTS_DIR);
    fs::create_dir_all(&agents).map_err(io_error("create", &agents))?;
    let (events, inputs) = sync_channel::<Input>(EVENT_QUEUE_MAX);
    let (jobs, queued) = sync_channel::<Job>(JOBS_QUEUED_MAX);
    let lease_millis = timing.lease_millis;
    let log = Log::start(&paths.home.join(LOG_FILE))?;
    let unaccounted = stop_leftovers(&paths.home, &timing, &log);
    let saved_cursor = read_cursor(&paths.home, &log);
    let recorder = start_recorder(paths.home.clone())?;
    match reap::find_leaks(&paths.board_url) {
        Ok(leaks) if leaks.is_empty() => {}
        Ok(leaks) => {
            for leak in &leaks {
                log.write(&format!("leaked {}", reap::leak_line(leak)));
            }
            let count = leaks.len();
            log.write(&format!(
                "{count} leaked vb processes; rider reap --yes stops them"
            ));
        }
        Err(error) => log.write(&format!("could not look for leaked vb processes: {error}")),
    }
    let socket_path = paths.home.join(SOCKET_FILE);
    let sink_events = events.clone();
    let sink: socket::Sink = Arc::new(move |event| send(&sink_events, event));
    let socket = socket::listen(&socket_path, paths.home.join(LOGS_DIR), sink)
        .map_err(io_error("listen on", &socket_path))?;
    let shared = Arc::new(Shared {
        paths,
        events,
        socket,
        lease_millis,
        grace_millis: timing.grace_millis,
        runner_seconds: timing.runner_millis / 1000,
        confine: confines_agents(&log),
        attempts: limits.attempts,
        feed: Mutex::new(FeedSlot::default()),
        served: Mutex::new(ServedSlot::default()),
        stop_now: AtomicBool::new(false),
        log,
        recorder,
        saved_cursor,
    });
    let board = BoardGuard(Arc::clone(&shared));
    serve_if_down(&shared);
    // Read before any claim, so no attempt spends its lease on it.
    let identity = board_identity_at_start(&shared.paths.board_url, &shared.board(DAEMON_PROFILE));
    if let Err(error) = identity {
        shared.log.write(&error.to_string());
        shared.socket.close();
        drop(board);
        return Err(DaemonError::Identity(error));
    }
    match register_schemas(&shared.board(DAEMON_PROFILE)) {
        Ok(registered) if registered.is_empty() => {}
        Ok(registered) => (shared.log).write(&format!("registered {}", registered.join(", "))),
        Err(error) => {
            let error = DaemonError::Schemas(error);
            shared.log.write(&error.to_string());
            shared.socket.close();
            drop(board);
            return Err(error);
        }
    }
    start_workers(queued, &shared.events)?;
    start_timer(shared.events.clone(), timing.tick_millis)?;
    start_signals(shared.events.clone())?;
    let mut config = Config::from_roles(roles);
    config.agents_max = limits.agents;
    config.builders_max = limits.builders_at_once;
    config.reconcile_millis = RECONCILE_SECONDS * 1000;
    config.lease_millis = lease_millis;
    config.attempt_millis_max = timing.attempt_millis_max.min(limits.attempt_millis);
    let mut state = State::new(config, now());
    state.leave_to_lease(unaccounted);
    let code = drive(&mut state, &inputs, &jobs, &shared);
    save_cursor(&shared, state.cursor());
    shared.socket.close();
    stop_feed(&shared);
    drop(board);
    let code = code?;
    assert!(state.exited());
    Ok(code)
}

// Starts the board as the daemon's own when it does not answer and its URL is on this host;
// a board that answers is left to whoever started it.
fn serve_if_down(shared: &Arc<Shared>) {
    let url = &shared.paths.board_url;
    let Some(port) = local_port(url) else { return };
    if keep_board(shared) {
        return;
    }
    match shared.board(DAEMON_PROFILE).list_open(SUBTREE) {
        Err(error) if call_failed(&error) => {}
        _ => return,
    }
    assert!(port.is_none_or(|port| !port.is_empty()));
    if let Err(error) = start_board(shared) {
        shared
            .log
            .write(&format!("starting the board failed: {error}"));
        send(&shared.events, Event::BoardExited);
    }
}

// The port of a board URL on this host, `Some(None)` when it names none; `None` for any
// other host.
fn local_port(url: &str) -> Option<Option<&str>> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(':') => (host, Some(port)),
        _ => (authority, None),
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    let port = port.filter(|port| !port.is_empty());
    let valid = port.is_none_or(|port| port.parse::<u16>().is_ok_and(|number| number > 0));
    valid.then_some(port)
}

// Starts `vb serve` in a process group of its own, with a thread that reports its exit
// unless the daemon is exiting.
fn start_board(shared: &Arc<Shared>) -> Result<(), DaemonError> {
    let mut slot = shared.served();
    if slot.stopping {
        return Ok(());
    }
    let home = &shared.paths.home;
    let path = home.join(BOARD_LOG_FILE);
    let log = OpenOptions::new().create(true).append(true).open(&path);
    let log = log.map_err(io_error("open", &path))?;
    let err = log.try_clone().map_err(io_error("share", &path))?;
    let vb = &shared.paths.vb;
    let mut command = board_serve(home, vb).map_err(io_error("create the board under", home))?;
    if let Some(Some(port)) = local_port(&shared.paths.board_url) {
        command.arg("--port").arg(port);
    }
    command
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .process_group(0);
    let mut child = command.spawn().map_err(io_error("run", vb))?;
    let group = Pid::from_child(&child);
    let watcher = Arc::clone(shared);
    let builder = thread::Builder::new().name("board".into());
    let spawned = builder.spawn(move || {
        let _ = child.wait();
        board_ended(&watcher);
    });
    let spawned = spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    shared
        .log
        .write(&format!("{BOARD_STARTED} {}", group.as_raw_nonzero()));
    record_board(shared, group);
    slot.group = Some(group);
    slot.thread = Some(spawned);
    assert!(!slot.stopping && slot.group.is_some());
    Ok(())
}

// Reports the end of the board the daemon owns unless the daemon is exiting.
fn board_ended(shared: &Shared) {
    let mut slot = shared.served();
    slot.group = None;
    if !slot.stopping {
        drop(slot);
        send(&shared.events, Event::BoardExited);
    }
}

// Names the board group `group` in the board file, so a daemon started after this one is
// killed keeps owning it and stops it.
fn record_board(shared: &Shared, group: Pid) {
    let path = shared.paths.home.join(BOARD_FILE);
    let Some(start) = start_time(group) else {
        shared
            .log
            .write("could not record the board: it already ended");
        return;
    };
    let record = WireBoard {
        group: group.as_raw_nonzero().get(),
        start,
    };
    assert!(record.group > 0);
    let written = serde_json::to_string(&record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        .and_then(|text| write_atomically(&path, text.as_bytes()));
    if let Err(error) = written {
        let line = format!("could not record the board in {}: {error}", path.display());
        shared.log.write(&line);
    }
}

// Takes on as the daemon's own the board the board file names while that process, started
// when the file says, still runs; a thread reports its end as an exit.
fn keep_board(shared: &Arc<Shared>) -> bool {
    let path = shared.paths.home.join(BOARD_FILE);
    let text = match read_bounded(&path, BOARD_FILE_BYTES_MAX) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(error) => {
            shared
                .log
                .write(&format!("could not read {}: {error}", path.display()));
            return false;
        }
    };
    let Ok(record) = serde_json::from_str::<WireBoard>(&text) else {
        shared
            .log
            .write(&format!("ignored {}: not a board record", path.display()));
        return false;
    };
    let Some(group) = Pid::from_raw(record.group) else {
        return false;
    };
    let start = record.start;
    let runs = move |group: Pid| {
        test_kill_process_group(group).is_ok() && start_time(group) == Some(start)
    };
    if !runs(group) {
        return false;
    }
    let mut slot = shared.served();
    assert!(slot.group.is_none() && !slot.stopping);
    let watcher = Arc::clone(shared);
    let builder = thread::Builder::new().name("board".into());
    let spawned = builder.spawn(move || {
        for _ in 0..BOARD_POLLS_MAX {
            if !runs(group) {
                break;
            }
            thread::sleep(Duration::from_millis(BOARD_POLL_MILLIS));
        }
        board_ended(&watcher);
    });
    let Ok(spawned) = spawned else {
        shared.log.write("could not watch the board started before");
        return false;
    };
    slot.group = Some(group);
    slot.thread = Some(spawned);
    shared
        .log
        .write(&format!("{BOARD_KEPT} {}", group.as_raw_nonzero()));
    true
}

/// Stops the board the daemon owns when dropped, so it ends on every exit, a panic included.
struct BoardGuard(Arc<Shared>);

impl Drop for BoardGuard {
    fn drop(&mut self) {
        stop_board(&self.0);
    }
}

// Stops the board the daemon started, SIGKILL after `BOARD_STOP_MILLIS_MAX`, and waits for
// its thread to reap it.
fn stop_board(shared: &Shared) {
    let (group, waiter) = {
        let mut slot = shared.served();
        slot.stopping = true;
        (slot.group.take(), slot.thread.take())
    };
    let (Some(group), Some(waiter)) = (group, waiter) else {
        return;
    };
    let _ = kill_process_group(group, Signal::TERM);
    let polls = BOARD_STOP_MILLIS_MAX / STOP_POLL_MILLIS;
    let ended = (0..polls).any(|_| {
        let finished = waiter.is_finished();
        if !finished {
            thread::sleep(Duration::from_millis(STOP_POLL_MILLIS));
        }
        finished
    });
    if ended {
        let _ = waiter.join();
    } else {
        let _ = kill_process_group(group, Signal::KILL);
        shared
            .log
            .write("the board did not end after SIGTERM; sent SIGKILL");
    }
    let path = shared.paths.home.join(BOARD_FILE);
    if let Err(error) = fs::remove_file(&path)
        && error.kind() != io::ErrorKind::NotFound
    {
        shared
            .log
            .write(&format!("could not remove {}: {error}", path.display()));
    }
}

// Kills the feed's group and waits up to `FEED_JOIN_MILLIS_MAX` for its thread to end, so
// the reader reaps `vb tail` before the daemon exits.
fn stop_feed(shared: &Shared) {
    let (stop, feed) = {
        let mut slot = shared.feed();
        slot.stopping = true;
        (slot.stop.take(), slot.thread.take())
    };
    // The thread is stored before it can take the lock to store its stop handle.
    assert!(
        stop.is_none() || feed.is_some(),
        "a feed's stop comes with its thread"
    );
    if let Some(Err(error)) = stop.as_ref().map(FeedStop::stop) {
        eprintln!("rider daemon: stopping the feed failed: {error}");
    }
    let Some(feed) = feed else { return };
    let polls = FEED_JOIN_MILLIS_MAX / STOP_POLL_MILLIS;
    for _ in 0..polls {
        if feed.is_finished() {
            let _ = feed.join();
            return;
        }
        thread::sleep(Duration::from_millis(STOP_POLL_MILLIS));
    }
    eprintln!("rider daemon: the feed did not end within {FEED_JOIN_MILLIS_MAX} ms");
}

fn drive(
    state: &mut State,
    inputs: &Receiver<Input>,
    jobs: &SyncSender<Job>,
    shared: &Arc<Shared>,
) -> Result<u8, DaemonError> {
    let mut contexts: Vec<RunContext> = Vec::with_capacity(usize::from(AGENTS_MAX));
    let mut following = false;
    let mut saved = shared.saved_cursor.unwrap_or(Cursor(0));
    let tick = step(state, Event::Tick(now()));
    let mut pending: Vec<Action> = tick;
    for _ in 0..STEPS_MAX {
        for action in pending.drain(..) {
            let action = match action {
                Action::Exit(code) => return Ok(u8::try_from(code).unwrap_or(1)),
                Action::Reply { client, reply } => {
                    shared.socket.reply(client, reply);
                    continue;
                }
                action => action,
            };
            if let Some(job) = dispatch(action, &mut contexts, shared) {
                jobs.try_send(job).map_err(|error| match error {
                    TrySendError::Full(_) => DaemonError::JobsFull,
                    TrySendError::Disconnected(_) => DaemonError::QueueClosed,
                })?;
            }
        }
        let input = inputs.recv().map_err(|_| DaemonError::QueueClosed)?;
        if !following && let Event::Snapshot { cursor, .. } = &input.event {
            // A saved cursor past the snapshot's is from another board, or one started anew.
            let from = shared.saved_cursor.filter(|saved| saved <= cursor);
            start_feed(shared, from.unwrap_or(*cursor))?;
            following = true;
            println!("{FOLLOWING} {SUBTREE} on {}", shared.paths.board_url);
        }
        let event = absorb(input, &mut contexts, &shared.log);
        let ticked = matches!(event, Event::Tick(_));
        let signalled = matches!(event, Event::Signal);
        pending = step(state, event);
        if signalled {
            let stopping = state.stopping();
            assert!(stopping != Stopping::No);
            shared.log.write(if stopping == Stopping::Now {
                STOP_NOW_LINE
            } else {
                DRAIN_LINE
            });
        }
        if ticked && state.cursor() != saved {
            saved = state.cursor();
            save_cursor(shared, saved);
        }
        let now_stopping = state.stopping() == Stopping::Now;
        shared.stop_now.store(now_stopping, Ordering::SeqCst);
        contexts.retain(|context| state.runs().iter().any(|run| run.id == context.run));
        assert!(contexts.len() <= usize::from(AGENTS_MAX));
    }
    Err(DaemonError::QueueClosed)
}

fn step(state: &mut State, event: Event) -> Vec<Action> {
    let actions = core::step(state, event);
    assert!(actions.len() <= ACTIONS_MAX);
    actions
}

// Keeps what a worker handed back, logs it, and returns the event for the core.
fn absorb(input: Input, contexts: &mut [RunContext], log: &Log) -> Event {
    let run = match &input.event {
        Event::Claimed { run, .. }
        | Event::Prepared { run, .. }
        | Event::AgentExited(run)
        | Event::Judged { run, .. }
        | Event::Settled(run) => Some(*run),
        _ => None,
    };
    let context = run.and_then(|run| contexts.iter_mut().find(|c| c.run == run));
    if let Some(context) = context {
        match (&input.event, input.extra) {
            (Event::Claimed { epoch, .. }, _) => {
                let line = format!(
                    "claimed #{} {} epoch {epoch}",
                    context.task.0, context.profile
                );
                log.write(&line);
            }
            (Event::Settled(_), _) => {
                log.write(&format!("settled #{} {}", context.task.0, context.profile));
            }
            (_, Extra::Attempt(attempt)) => context.attempt = Some(attempt),
            (_, Extra::Launched(launched)) => context.launched = Some(launched),
            (_, Extra::Judgement(judgement)) => context.judgement = Some(judgement),
            (_, Extra::None) => {}
        }
    }
    input.event
}

// The job carrying out `action`; every action but `Exit` gets one, which answers it once.
fn dispatch(action: Action, contexts: &mut Vec<RunContext>, shared: &Arc<Shared>) -> Option<Job> {
    assert!(contexts.len() <= usize::from(AGENTS_MAX));
    if let Action::Claim { run, task, profile } | Action::Resume { run, task, profile } = &action {
        contexts.push(RunContext::new(*run, *task, profile.clone()));
    }
    match job(action, contexts, shared) {
        Ok(job) => job,
        Err(event) => {
            shared
                .log
                .write(&format!("lost a run's state; answered {event:?}"));
            Some(Box::new(move || Input::plain(event)))
        }
    }
}

// The job for `action`, or the failure that answers it when its run's context is missing.
fn job(
    action: Action,
    contexts: &mut [RunContext],
    shared: &Arc<Shared>,
) -> Result<Option<Job>, Event> {
    let shared = Arc::clone(shared);
    let lost = |run: RunId, during: &str| Event::PrepareBlocked {
        run,
        kind: "rider_error".to_string(),
        reason: format!("Rider lost the run's state before it could {during} the attempt."),
    };
    let job: Job = match action {
        Action::Claim { run, task, profile } => {
            Box::new(move || claim(&shared, run, task, &profile))
        }
        Action::Resume { run, task, profile } => {
            Box::new(move || resume(&shared, run, task, &profile))
        }
        Action::Prepare { run, task } => {
            let profile = find(contexts, run, lost(run, "prepare"))?.profile.clone();
            Box::new(move || prepare(&shared, run, task, &profile))
        }
        Action::Spawn { run, task, profile } => {
            let context = find(contexts, run, Event::AgentExited(run))?;
            let attempt = context.attempt.clone().ok_or(Event::AgentExited(run))?;
            shared.log.write(&format!("spawned #{} {profile}", task.0));
            Box::new(move || spawn(&shared, run, task, &profile, &attempt))
        }
        Action::StopAgent { run, task } => {
            let context = contexts.iter().find(|context| context.run == run);
            let attempt = context.and_then(|context| context.attempt.clone());
            Box::new(move || stop_agent(&shared, run, task, attempt.as_deref()))
        }
        Action::Judge { run, task } => {
            let reason = "rider lost the attempt's state before judging it".to_string();
            let unjudged = Event::Judged {
                run,
                verdict: Verdict::Fail { reason },
            };
            let context = find(contexts, run, unjudged.clone())?;
            let attempt = context.attempt.clone().ok_or(unjudged.clone())?;
            let launched = context.launched.take().ok_or(unjudged)?;
            let profile = context.profile.clone();
            Box::new(move || judge(&shared, run, task, &profile, &attempt, launched))
        }
        Action::Renew { run, task } => {
            let profile = find(contexts, run, Event::BoardFailed(Call::Renew(run)))?
                .profile
                .clone();
            Box::new(move || renew(&shared, run, task, &profile))
        }
        Action::Settle {
            run,
            task,
            settlement,
            ..
        } => {
            let context = find(contexts, run, Event::BoardFailed(Call::Settle(run)))?;
            let close = Close {
                settlement,
                attempt: context.attempt.clone(),
                judgement: context.judgement.clone(),
            };
            let profile = context.profile.clone();
            Box::new(move || settle(&shared, run, task, &profile, close))
        }
        Action::Release {
            run, task, profile, ..
        } => Box::new(move || release(&shared, run, task, &profile)),
        Action::ListTasks => Box::new(move || list(&shared)),
        Action::RestartFeed(cursor) => Box::new(move || restart_feed(&shared, cursor)),
        Action::RestartBoard => Box::new(move || match start_board(&shared) {
            Ok(()) => Input::plain(Event::BoardRestarted),
            Err(error) => {
                shared
                    .log
                    .write(&format!("restarting the board failed: {error}"));
                Input::plain(Event::BoardExited)
            }
        }),
        Action::Reply { .. } | Action::Exit(_) => return Ok(None),
    };
    Ok(Some(job))
}

fn find(contexts: &mut [RunContext], run: RunId, missing: Event) -> Result<&mut RunContext, Event> {
    let found = contexts.iter_mut().find(|context| context.run == run);
    assert!(found.as_ref().is_none_or(|context| context.run == run));
    found.ok_or(missing)
}

fn call_failed(error: &BoardError) -> bool {
    matches!(
        error,
        BoardError::Spawn(_)
            | BoardError::Io(_)
            | BoardError::TimedOut { .. }
            | BoardError::Unreachable { .. }
    )
}

fn claim(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    assert!(!profile.is_empty());
    let sent = now();
    let claimed = shared.board(profile).claim_task(task);
    let event = match claimed {
        Ok(view) => match view.claim {
            Some(claim) if view.id == task => Event::Claimed {
                run,
                epoch: view.epoch,
                until: shared.lease_until(sent, claim.until),
            },
            _ => Event::ClaimRefused(run),
        },
        Err(error) if call_failed(&error) => Event::BoardFailed(Call::Claim(run)),
        Err(_) => Event::ClaimRefused(run),
    };
    Input::plain(event)
}

// Renews a claim held from before a restart; it is answered as a claim, keeping its epoch.
fn resume(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    assert!(!profile.is_empty());
    let sent = now();
    let renewed = shared.board(profile).renew(task);
    let event = match renewed {
        Ok(view) => match view.claim {
            Some(claim) if view.id == task => Event::Claimed {
                run,
                epoch: view.epoch,
                until: shared.lease_until(sent, claim.until),
            },
            _ => Event::ClaimRefused(run),
        },
        Err(error) if call_failed(&error) => Event::BoardFailed(Call::Claim(run)),
        // `claim_lost` or `claim_not_held`: the task becomes ready when its lease lapses.
        Err(_) => Event::ClaimRefused(run),
    };
    let outcome = match &event {
        Event::Claimed { .. } => "renewed",
        Event::BoardFailed(_) => "the board did not answer",
        _ => "no longer held",
    };
    shared
        .log
        .write(&format!("resumed #{} {profile}: {outcome}", task.0));
    Input::plain(event)
}

fn prepare(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    let board = shared.board(profile);
    let prepared = prepare_attempt(task, &shared.env(profile), &board);
    match prepared {
        Ok(Prepared::Ready(attempt)) => {
            let branch = attempt.branch().map(str::to_string);
            let role = attempt.role().label.to_string();
            let attempt = *attempt;
            Input {
                event: Event::Prepared { run, branch, role },
                extra: Extra::Attempt(Arc::new(attempt)),
            }
        }
        Ok(Prepared::Blocked { kind, reason }) => Input::plain(Event::PrepareBlocked {
            run,
            kind: kind.to_string(),
            reason,
        }),
        Err(error) => Input::plain(run_error_event(run, &error, "prepare")),
    }
}

fn run_error_event(run: RunId, error: &RunError, during: &str) -> Event {
    assert!(!during.is_empty());
    match error {
        RunError::LostClaim(_) | RunError::Board(BoardError::ClaimLost { .. }) => {
            Event::ClaimLost(run)
        }
        // Any other board error stops this step; the closing write releases the claim.
        RunError::Board(_) => Event::PrepareBlocked {
            run,
            kind: "rider_error".to_string(),
            reason: format!(
                "Rider could not {during} the attempt: {}.",
                run_error_text(error)
            ),
        },
    }
}

fn run_error_text(error: &RunError) -> String {
    match error {
        RunError::Board(error) => error.to_string(),
        RunError::LostClaim(state) => format!("the task is no longer claimed; it is {state:?}"),
    }
}

fn spawn(shared: &Shared, run: RunId, task: TaskId, profile: &str, attempt: &Attempt) -> Input {
    let pid_file = shared.pid_file(task);
    let vb_dir = shared
        .paths
        .vb
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned());
    let vb_dir = vb_dir.unwrap_or_else(|| "/".to_string());
    let pid_text = pid_file.to_string_lossy().into_owned();
    let ack = ack_file(&pid_file);
    let ack_text = ack.to_string_lossy().into_owned();
    let stop = stop_file(&pid_file);
    let stop_text = stop.to_string_lossy().into_owned();
    let runner_seconds = shared.runner_seconds;
    let home = shared.paths.home.clone();
    let vb = shared.paths.vb.clone();
    let confine = shared.confine;
    // A stale acknowledgement must never release a new agent whose pid happens to match it.
    let _ = fs::remove_file(&ack);
    let _ = fs::remove_file(&stop);
    let launch = move |cmd: &CommandLine, dir: &Path, log: &Path| {
        let mut args = vec!["-c".to_string(), PID_WRAPPER.to_string(), "sh".to_string()];
        if confine {
            args.extend(confinement(&home, &vb)?);
        }
        args.push(cmd.program.clone());
        args.extend(cmd.args.iter().cloned());
        let mut env = cmd.env.clone();
        env.push(("RIDER_AGENT_VB_DIR".to_string(), vb_dir.clone()));
        env.push(("RIDER_AGENT_PID_FILE".to_string(), pid_text.clone()));
        env.push(("RIDER_AGENT_ACK_FILE".to_string(), ack_text.clone()));
        env.push(("RIDER_AGENT_STOP_FILE".to_string(), stop_text.clone()));
        let wrapped = CommandLine {
            program: "/bin/sh".to_string(),
            args,
            env,
            io: cmd.io.clone(),
        };
        launch_agent(&wrapped, dir, log, runner_seconds)
    };
    // A full queue makes the worker wait: the recorder is never far behind.
    let _ = shared.recorder.send(Recorded::Started(task));
    let launched = attempt.launch(&shared.board(profile), &launch);
    // The agent is gone, so its pid may be reused: nothing may signal it any more.
    let _ = fs::remove_file(&ack);
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(stop_file(&pid_file));
    let _ = shared.recorder.send(Recorded::Ended(task));
    Input {
        event: Event::AgentExited(run),
        extra: Extra::Launched(launched),
    }
}

// macOS still ships the deprecated sandbox-exec. Every process a confined agent starts,
// detached or in a group of its own, inherits the profile and cannot leave it.
#[cfg(target_os = "macos")]
fn confinement(home: &Path, vb: &Path) -> Result<Vec<String>, HarnessError> {
    let unusable = |why: String| HarnessError::Spawn(format!("cannot confine the agent: {why}"));
    let home = fs::canonicalize(home)
        .map_err(|error| unusable(format!("resolve {}: {error}", home.display())))?;
    let quote = |path: &Path| match path.to_str() {
        Some(text) if !text.contains(['"', '\\']) && !text.contains(char::is_control) => {
            Ok(format!("\"{text}\""))
        }
        _ => Err(unusable(format!(
            "{} cannot be named in a profile",
            path.display()
        ))),
    };
    if home.components().count() > HOME_DEPTH_MAX {
        return Err(unusable(format!("{} is too deep", home.display())));
    }
    let vb = fs::canonicalize(vb)
        .map_err(|error| unusable(format!("resolve {}: {error}", vb.display())))?;
    let mut denied = Vec::with_capacity(HOME_DEPTH_MAX + 2 * CONTROL_FILES.len() + 4);
    for dir in home.ancestors() {
        denied.push(format!("(literal {})", quote(dir)?));
    }
    for name in CONTROL_FILES {
        let staged = format!("{name}{STAGED_SUFFIX}");
        denied.push(format!("(literal {})", quote(&home.join(name))?));
        denied.push(format!("(literal {})", quote(&home.join(staged))?));
    }
    denied.push(format!("(literal {})", quote(&vb)?));
    for name in [AGENTS_DIR, LOGS_DIR, NODE_DIR] {
        denied.push(format!("(subpath {})", quote(&home.join(name))?));
    }
    let socket = quote(&home.join(SOCKET_FILE))?;
    let profile = format!(
        "(version 1)\n(allow default)\n\
         (deny network-outbound (remote unix-socket (path-literal {socket})))\n\
         (deny file-write* {})\n",
        denied.join(" ")
    );
    assert!(denied.len() > 2 * CONTROL_FILES.len() && profile.contains(&socket));
    Ok(vec![
        SANDBOX_EXEC.to_string(),
        "-p".to_string(),
        profile,
        "--".to_string(),
    ])
}

// sandbox-exec exits 71, printing `sandbox_apply: Operation not permitted`, when the process
// that starts it is already in a sandbox: macOS refuses to apply a second profile that denies
// anything, though it accepts one that only allows.
#[cfg(target_os = "macos")]
fn confines_agents(log: &Log) -> bool {
    let probe = Command::new(SANDBOX_EXEC)
        .args(["-p", SANDBOX_PROBE, "--", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let nested = probe.is_ok_and(|status| status.code() == Some(SANDBOX_REFUSED_EXIT));
    if nested {
        log.write(
            "agents run unconfined: the daemon runs in a sandbox, \
             and macOS does not let it confine an agent again",
        );
    }
    !nested
}

#[cfg(not(target_os = "macos"))]
fn confines_agents(log: &Log) -> bool {
    let _ = log;
    false
}

#[cfg(not(target_os = "macos"))]
fn confinement(home: &Path, vb: &Path) -> Result<Vec<String>, HarnessError> {
    assert!(home.is_absolute() && !vb.as_os_str().is_empty());
    Ok(Vec::new())
}

fn stop_agent(shared: &Shared, run: RunId, task: TaskId, attempt: Option<&Attempt>) -> Input {
    assert!(shared.grace_millis > 0 && shared.grace_millis <= GRACE_MILLIS);
    let path = shared.pid_file(task);
    if let Some(attempt) = attempt {
        attempt.stop();
        // The marker goes before the pid file is read, and the wrapper writes its pid before
        // it looks for the marker, so an agent starting now either sees it or is signalled.
        if let Err(error) = fs::write(stop_file(&path), "") {
            let line = format!("could not mark #{} stopped: {error}", task.0);
            shared.log.write(&line);
        }
    }
    let mut stopped = None;
    // A refused resume's fresh start may have begun before the attempt was told to stop.
    for _ in 0..AGENT_GROUPS_MAX {
        let Some(group) = agent_group(&path).filter(|group| Some(*group) != stopped) else {
            break;
        };
        // A group that is already gone has nothing left to stop.
        let _ = kill_process_group(group, Signal::TERM);
        let polls = shared.grace_millis.div_ceil(STOP_POLL_MILLIS);
        let ended = (0..polls).any(|_| {
            thread::sleep(Duration::from_millis(STOP_POLL_MILLIS));
            // The pid file names another group once a fresh start began, and goes once the
            // attempt's last agent is reaped.
            agent_group(&path) != Some(group)
        });
        if !ended {
            let _ = kill_process_group(group, Signal::KILL);
        }
        stopped = Some(group);
    }
    assert!(stopped.is_none_or(|group| group.as_raw_nonzero().get() > 1));
    Input::plain(Event::StopDone(run))
}

// The process group of the agent whose pid file is `path`, while it has not been reaped.
fn agent_group(path: &Path) -> Option<Pid> {
    let mut text = String::new();
    let read = File::open(path).and_then(|file| file.take(PID_BYTES_MAX).read_to_string(&mut text));
    let group = read.ok().and_then(|_| text.trim().parse::<i32>().ok());
    group.filter(|pid| *pid > 1).and_then(Pid::from_raw)
}

fn judge(
    shared: &Shared,
    run: RunId,
    task: TaskId,
    profile: &str,
    attempt: &Attempt,
    launched: Result<Exit, HarnessError>,
) -> Input {
    let board = shared.board(profile);
    match judge_attempt(task, &board, attempt, launched) {
        Ok(judgement) => Input {
            event: Event::Judged {
                run,
                verdict: judgement.verdict.clone(),
            },
            extra: Extra::Judgement(Arc::new(judgement)),
        },
        Err(RunError::Board(error)) if !matches!(error, BoardError::ClaimLost { .. }) => {
            let reason = format!("rider could not judge the attempt: {error}");
            Input::plain(Event::Judged {
                run,
                verdict: Verdict::Fail { reason },
            })
        }
        Err(_) => Input::plain(Event::ClaimLost(run)),
    }
}

fn renew(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    let sent = now();
    let event = match shared.board(profile).renew(task) {
        Ok(view) => match view.claim {
            Some(claim) => Event::Renewed {
                run,
                until: shared.lease_until(sent, claim.until),
            },
            None => Event::ClaimLost(run),
        },
        Err(error) if call_failed(&error) => Event::BoardFailed(Call::Renew(run)),
        Err(_) => Event::ClaimLost(run),
    };
    Input::plain(event)
}

struct Close {
    settlement: Settlement,
    attempt: Option<Arc<Attempt>>,
    judgement: Option<Arc<Judgement>>,
}

fn settle(shared: &Shared, run: RunId, task: TaskId, profile: &str, close: Close) -> Input {
    let board = Guarded {
        board: shared.board(profile),
        shared,
        task,
        counted: Cell::new(false),
        refused: Cell::new(false),
        noted: Cell::new(false),
    };
    let written = match (&close.settlement, &close.attempt, &close.judgement) {
        _ if board.stopped() => {
            board.refused.set(true);
            Err(RunError::Board(BoardError::Io(STOPPED.to_string())))
        }
        (Settlement::Block { kind, reason }, _, _) => {
            board.block(task, reason, kind).map_err(RunError::Board)
        }
        (Settlement::Verdict(_), Some(attempt), Some(judgement)) => {
            settle_attempt(task, &shared.env(profile), &board, attempt, judgement)
        }
        // A failure decided without judging, such as the time limit, still counts the attempt.
        (Settlement::Verdict(verdict @ Verdict::Fail { .. }), Some(attempt), None) => {
            match board.read_task(task).map(|view| view.state) {
                Ok(TaskState::Claimed) => {
                    let judgement = Judgement::uninspected(verdict.clone());
                    settle_attempt(task, &shared.env(profile), &board, attempt, &judgement)
                }
                // A worker that blocked its task owns it now, however its process ended.
                Ok(TaskState::Blocked) => Ok(()),
                Ok(state) => Err(RunError::LostClaim(state)),
                Err(error) => Err(RunError::Board(error)),
            }
        }
        (Settlement::Verdict(Verdict::Fail { reason }), _, _) => {
            board.fail(task, reason).map_err(RunError::Board)
        }
        (Settlement::Verdict(_), _, _) => Ok(()),
        (Settlement::Release, _, _) => board.release(task).map_err(RunError::Board),
    };
    if board.refused.get() {
        shared.log.write(&format!(
            "released #{} {profile} instead of settling: stop-now",
            task.0
        ));
        return release(shared, run, task, profile);
    }
    let event = match written {
        Ok(()) => Event::Settled(run),
        Err(RunError::Board(error)) if call_failed(&error) => Event::BoardFailed(Call::Settle(run)),
        Err(RunError::Board(BoardError::ClaimLost { .. }) | RunError::LostClaim(_)) => {
            Event::CloseLost(run)
        }
        Err(RunError::Board(_)) => Event::BoardFailed(Call::Settle(run)),
    };
    Input::plain(event)
}

/// The board a settlement writes through: on stop-now it refuses every write until one that
/// settles or counts the attempt has been made, and after that it lets the rest finish.
struct Guarded<'a> {
    board: VbCli,
    shared: &'a Shared,
    task: TaskId,
    /// A write that settles the task or counts its attempt was sent; it cannot be undone.
    counted: Cell<bool>,
    /// A write was refused for stop-now.
    refused: Cell<bool>,
    /// The log says this settlement finishes despite stop-now.
    noted: Cell<bool>,
}

impl Guarded<'_> {
    fn stopped(&self) -> bool {
        self.shared.stop_now.load(Ordering::SeqCst)
    }

    fn gate(&self, counts: bool) -> Result<(), BoardError> {
        assert!(!self.refused.get() || !self.counted.get());
        if self.stopped() {
            if !self.counted.get() {
                self.refused.set(true);
                return Err(BoardError::Io(STOPPED.to_string()));
            }
            if !self.noted.replace(true) {
                let line = format!(
                    "finishing the settlement of #{} after stop-now: it was written before",
                    self.task.0
                );
                self.shared.log.write(&line);
            }
        }
        self.counted.set(self.counted.get() || counts);
        assert!(!counts || self.counted.get());
        Ok(())
    }
}

impl Board for Guarded<'_> {
    fn read_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
        self.board.read_task(id)
    }
    fn claim_ready(
        &self,
        channel: &str,
        role: &str,
        wait: u32,
    ) -> Result<Option<TaskId>, BoardError> {
        self.board.claim_ready(channel, role, wait)
    }
    fn set_label(&self, id: TaskId, key: &str, value: &str) -> Result<(), BoardError> {
        self.gate(true)?;
        self.board.set_label(id, key, value)
    }
    fn reply(&self, id: TaskId, text: &str) -> Result<(), BoardError> {
        self.gate(false)?;
        self.board.reply(id, text)
    }
    fn post_structured(&self, id: TaskId, schema: &str, json: &str) -> Result<(), BoardError> {
        self.gate(false)?;
        self.board.post_structured(id, schema, json)
    }
    fn complete(&self, id: TaskId, result: &str) -> Result<(), BoardError> {
        self.gate(true)?;
        self.board.complete(id, result)
    }
    fn fail(&self, id: TaskId, reason: &str) -> Result<(), BoardError> {
        self.gate(true)?;
        self.board.fail(id, reason)
    }
    fn block(&self, id: TaskId, reason: &str, kind: &str) -> Result<(), BoardError> {
        self.gate(true)?;
        self.board.block(id, reason, kind)
    }
    fn claim_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
        self.board.claim_task(id)
    }
    fn renew(&self, id: TaskId) -> Result<TaskView, BoardError> {
        self.board.renew(id)
    }
    fn release(&self, id: TaskId) -> Result<(), BoardError> {
        self.board.release(id)
    }
    fn list_open(&self, subtree: &str) -> Result<Listing, BoardError> {
        self.board.list_open(subtree)
    }
    fn identity(&self) -> Result<String, BoardError> {
        self.board.identity()
    }
    fn list_labelled(
        &self,
        channel: &str,
        key: &str,
        values: &[&str],
    ) -> Result<Listing, BoardError> {
        self.board.list_labelled(channel, key, values)
    }
    fn thread(&self, id: TaskId) -> Result<Thread, BoardError> {
        self.board.thread(id)
    }
    fn blocking_findings(&self, id: TaskId) -> Result<usize, BoardError> {
        self.board.blocking_findings(id)
    }
    fn schema_ids(&self) -> Result<Vec<String>, BoardError> {
        self.board.schema_ids()
    }
    fn create_schema(&self, id: &str, json: &str) -> Result<(), BoardError> {
        self.board.create_schema(id, json)
    }
}

fn release(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    let event = match shared.board(profile).release(task) {
        Ok(()) => Event::Settled(run),
        Err(BoardError::ClaimLost { .. }) => Event::CloseLost(run),
        Err(_) => Event::BoardFailed(Call::Release(run)),
    };
    Input::plain(event)
}

fn list(shared: &Shared) -> Input {
    let event = match shared.board(DAEMON_PROFILE).list_open(SUBTREE) {
        Ok(listing) => Event::Snapshot {
            tasks: listing.tasks,
            cursor: listing.cursor,
            left_out: listing.left_out,
        },
        // No `work/` channel exists yet: nothing is open, and the feed covers every event.
        Err(BoardError::Exit { stderr, .. })
            if parse_failure_reason(&stderr).as_deref() == Some(NO_CHANNEL) =>
        {
            Event::Snapshot {
                tasks: Vec::new(),
                cursor: Cursor(0),
                left_out: 0,
            }
        }
        Err(error) => {
            eprintln!("rider daemon: listing {SUBTREE} failed: {error}");
            Event::BoardFailed(Call::ListTasks)
        }
    };
    Input::plain(event)
}

fn restart_feed(shared: &Arc<Shared>, cursor: Cursor) -> Input {
    match start_feed(shared, cursor) {
        Ok(()) => Input::plain(Event::FeedResumed(cursor)),
        Err(_) => Input::plain(Event::FeedLost),
    }
}

// Starts a thread that follows the feed after `cursor` and reports its loss once, unless
// the daemon is exiting.
fn start_feed(shared: &Arc<Shared>, cursor: Cursor) -> Result<(), DaemonError> {
    let mut slot = shared.feed();
    if slot.stopping {
        return Ok(());
    }
    let feed = Arc::clone(shared);
    let builder = thread::Builder::new().name("feed".into());
    let spawned = builder.spawn(move || {
        follow(&feed, cursor);
        let mut slot = feed.feed();
        slot.stop = None;
        if !slot.stopping {
            drop(slot);
            send(&feed.events, Event::FeedLost);
        }
    });
    let spawned = spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    slot.thread = Some(spawned);
    assert!(!slot.stopping && slot.thread.is_some());
    Ok(())
}

// Follows the feed until it ends, the daemon stops it, or nobody listens.
fn follow(shared: &Shared, cursor: Cursor) {
    let board = shared.board(DAEMON_PROFILE);
    let events = &shared.events;
    let Ok(mut reader) = FeedReader::start(&board, cursor) else {
        return;
    };
    {
        let mut slot = shared.feed();
        if slot.stopping {
            return;
        }
        slot.stop = Some(reader.stopper());
    }
    for _ in 0..FEED_ITEMS_MAX {
        match reader.next_item() {
            Ok(FeedItem::Event(FeedEvent::Known(event))) => {
                if event.channel.starts_with(SUBTREE) && !send(events, Event::Feed(event)) {
                    return;
                }
            }
            Ok(FeedItem::Event(FeedEvent::Other { .. }) | FeedItem::Idle) => {}
            Err(FeedError::LineTooLong | FeedError::Parse(_) | FeedError::TooManyLabels) => {}
            Ok(FeedItem::Exited { .. }) | Err(_) => return,
        }
    }
}

fn send(events: &SyncSender<Input>, event: Event) -> bool {
    events.send(Input::plain(event)).is_ok()
}

fn start_workers(queued: Receiver<Job>, events: &SyncSender<Input>) -> Result<(), DaemonError> {
    let queued = Arc::new(Mutex::new(queued));
    for index in 0..WORKERS_MAX {
        let (queued, events) = (Arc::clone(&queued), events.clone());
        let builder = thread::Builder::new().name(format!("worker-{index}"));
        let spawned = builder.spawn(move || {
            for _ in 0..JOBS_MAX {
                let job = match queued.lock() {
                    Ok(receiver) => receiver.recv(),
                    Err(_) => return,
                };
                let Ok(job) = job else { return };
                if events.send(job()).is_err() {
                    return;
                }
            }
        });
        spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    }
    Ok(())
}

fn start_timer(events: SyncSender<Input>, tick_millis: u64) -> Result<(), DaemonError> {
    assert!(tick_millis > 0);
    let builder = thread::Builder::new().name("timer".into());
    let spawned = builder.spawn(move || {
        for _ in 0..TICKS_MAX {
            thread::sleep(Duration::from_millis(tick_millis));
            if !send(&events, Event::Tick(now())) {
                return;
            }
        }
    });
    spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    assert!(!thread::panicking());
    Ok(())
}

fn start_signals(events: SyncSender<Input>) -> Result<(), DaemonError> {
    let mut signals =
        Signals::new([SIGTERM, SIGINT]).map_err(|error| DaemonError::Thread(error.to_string()))?;
    let builder = thread::Builder::new().name("signals".into());
    let spawned = builder.spawn(move || {
        for _ in signals.forever().take(SIGNALS_MAX) {
            if !send(&events, Event::Signal) {
                return;
            }
        }
    });
    spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    Ok(())
}

fn now() -> Timestamp {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let millis = since.map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    });
    Timestamp(millis)
}

/// The daemon's log, written by its own thread so a slow file never holds up the core.
struct Log {
    lines: SyncSender<String>,
    /// Lines dropped because the queue was full, reported with the next line written.
    dropped: Arc<AtomicU64>,
}

impl Log {
    // Anything there but a regular file is refused before opening, which would block on a FIFO.
    fn start(path: &Path) -> Result<Log, DaemonError> {
        match fs::metadata(path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(DaemonError::NotAFile(path.to_path_buf()));
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(io_error("inspect", path)(error));
            }
            Ok(_) | Err(_) => {}
        }
        let opened = OpenOptions::new().create(true).append(true).open(path);
        let file = opened.map_err(io_error("open", path))?;
        let metadata = file.metadata().map_err(io_error("inspect", path))?;
        if !metadata.is_file() {
            return Err(DaemonError::NotAFile(path.to_path_buf()));
        }
        let (lines, queued) = sync_channel::<String>(LOG_QUEUE_MAX);
        let dropped = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&dropped);
        let builder = thread::Builder::new().name("log".into());
        let spawned = builder.spawn(move || write_lines(file, &queued, &counted));
        spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
        Ok(Log { lines, dropped })
    }

    // Queues a timestamped line; a full queue drops and counts it rather than wait.
    fn write(&self, line: &str) {
        assert!(!line.contains('\n'));
        let text = format!("{} {line}\n", clock_line(SystemTime::now()));
        assert!(text.ends_with('\n'));
        if let Err(TrySendError::Full(_)) = self.lines.try_send(text) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// Appends queued lines to `file` until the daemon drops its log; losing one must not stop it.
fn write_lines(mut file: File, queued: &Receiver<String>, dropped: &AtomicU64) {
    for _ in 0..LOG_LINES_MAX {
        let Ok(text) = queued.recv() else { return };
        let lost = dropped.swap(0, Ordering::SeqCst);
        if lost > 0 {
            let note = format!(
                "{} dropped {lost} log lines\n",
                clock_line(SystemTime::now())
            );
            let _ = file.write_all(note.as_bytes());
        }
        let _ = file.write_all(text.as_bytes());
    }
}

// `now` as UTC `YYYY-MM-DDTHH:MM:SSZ`.
fn clock_line(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, day) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01, after Howard Hinnant's algorithm.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let doe = shifted - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (day / 3600, day % 3600 / 60, day % 60);
    let line = format!("{year:04}-{month:02}-{date:02}T{hour:02}:{minute:02}:{second:02}Z");
    assert!(line.len() >= "YYYY-MM-DDTHH:MM:SSZ".len());
    assert!((1..=12).contains(&month));
    line
}

/// One running agent as `agents.json` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAgent {
    task: u64,
    group: i32,
    /// When the group's leader started, in seconds since the epoch.
    start: u64,
}

/// The board process the daemon started as the board file names it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBoard {
    group: i32,
    /// When the board process started, in seconds since the epoch.
    start: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsFile {
    agents: Vec<WireAgent>,
}

/// What the recorder is told by the workers that run agents.
enum Recorded {
    Started(TaskId),
    Ended(TaskId),
}

/// Writes `bytes` to `path` by renaming a finished file beside it over it.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(STAGED_SUFFIX);
    let staged = path.with_file_name(name);
    assert!(staged != path);
    fs::write(&staged, bytes)?;
    fs::rename(&staged, path)
}

fn read_bounded(path: &Path, bytes_max: u64) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(bytes_max.saturating_add(1))
        .read_to_string(&mut text)?;
    let length = u64::try_from(text.len()).unwrap_or(u64::MAX);
    if length > bytes_max {
        let message = format!("longer than {bytes_max} bytes");
        return Err(io::Error::new(io::ErrorKind::InvalidData, message));
    }
    Ok(text)
}

// The cursor saved under `home`; one that is missing is `None`, and so is a malformed one,
// which is logged.
fn read_cursor(home: &Path, log: &Log) -> Option<Cursor> {
    let path = home.join(CURSOR_FILE);
    let text = match read_bounded(&path, CURSOR_BYTES_MAX) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            log.write(&format!("could not read {}: {error}", path.display()));
            return None;
        }
    };
    let cursor = text.trim().parse::<u64>().ok().map(Cursor);
    if cursor.is_none() {
        log.write(&format!("ignored {}: not a cursor", path.display()));
    }
    cursor
}

fn save_cursor(shared: &Shared, cursor: Cursor) {
    let path = shared.paths.home.join(CURSOR_FILE);
    if let Err(error) = write_atomically(&path, format!("{}\n", cursor.0).as_bytes()) {
        let line = format!("could not save the cursor to {}: {error}", path.display());
        shared.log.write(&line);
    }
}

// When process `pid` started, in seconds since the epoch, while it exists.
fn start_time(pid: Pid) -> Option<u64> {
    let raw = u32::try_from(pid.as_raw_nonzero().get()).ok()?;
    let pid = sysinfo::Pid::from_u32(raw);
    let mut system = System::new().ok()?;
    let only = [pid];
    let kind = ProcessRefreshKind::nothing();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&only), true, kind);
    let start = system.process(pid).map(sysinfo::Process::start_time);
    start.filter(|start| *start > 0)
}

// The agents `agents.json` under `home` names; a missing file names none.
fn read_agents(home: &Path) -> Result<Vec<AgentRecord>, String> {
    let path = home.join(AGENTS_FILE);
    let text = match read_bounded(&path, AGENTS_BYTES_MAX) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let file: AgentsFile = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if file.agents.len() > AGENT_RECORDS_MAX {
        return Err(format!("names more than {AGENT_RECORDS_MAX} agents"));
    }
    let records = file.agents.iter().map(|agent| AgentRecord {
        task: TaskId(agent.task),
        group: agent.group,
        start: agent.start,
    });
    Ok(records.collect())
}

// The pid files under `home`, each with its acknowledgement; a missing directory holds none.
fn read_pid_files(home: &Path) -> Result<Vec<PidFile>, String> {
    let entries = match fs::read_dir(home.join(AGENTS_DIR)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut files = Vec::new();
    for (count, entry) in entries.enumerate() {
        if count >= AGENT_FILES_MAX {
            return Err(format!("more than {AGENT_FILES_MAX} files"));
        }
        let path = entry.map_err(|error| error.to_string())?.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if !matches!(extension, Some("pid" | LEFT_EXTENSION)) {
            continue;
        }
        let stem = path.file_stem().and_then(|stem| stem.to_str());
        let Some(task) = stem.and_then(|stem| stem.parse::<u64>().ok()).map(TaskId) else {
            continue;
        };
        let group = agent_group(&path).map(|group| group.as_raw_nonzero().get());
        let acked = (extension == Some("pid")).then(|| read_ack(&ack_file(&path)));
        let ack = acked
            .flatten()
            .map(|(group, start)| AgentRecord { task, group, start });
        files.push(PidFile { task, group, ack });
    }
    assert!(files.len() <= AGENT_FILES_MAX);
    Ok(files)
}

// The group and start time an ack file names, while it is there and well formed.
fn read_ack(path: &Path) -> Option<(i32, u64)> {
    let text = read_bounded(path, ACK_BYTES_MAX).ok()?;
    let (group, start) = text.trim().split_once(' ')?;
    Some((group.parse().ok()?, start.parse().ok()?))
}

/// Stops each agent a crashed daemon left running, only a group whose leader started when a
/// record says, so a reused pid is never signalled; returns the tasks to leave to their leases.
fn stop_leftovers(home: &Path, timing: &Timing, log: &Log) -> Unaccounted {
    let grace_millis = timing.grace_millis;
    assert!(grace_millis > 0);
    let records = read_agents(home)
        .inspect_err(|error| log.write(&format!("could not read {AGENTS_FILE}: {error}")))
        .ok();
    let pid_files = read_pid_files(home)
        .inspect_err(|error| log.write(&format!("could not read the pid files: {error}")))
        .ok();
    let started = |group: i32| {
        let group = Pid::from_raw(group).filter(|group| test_kill_process_group(*group).is_ok());
        group.and_then(start_time)
    };
    let recovery = core::recover(records.as_deref(), pid_files.as_deref(), &started);
    let mut groups: Vec<(Pid, u64)> = Vec::with_capacity(recovery.stop.len());
    for record in &recovery.stop {
        let Some(group) = Pid::from_raw(record.group) else {
            continue;
        };
        let (task, raw) = (record.task.0, record.group);
        log.write(&format!("stopping #{task} left from before: group {raw}"));
        let _ = kill_process_group(group, Signal::TERM);
        groups.push((group, record.start));
    }
    let polls = grace_millis.div_ceil(STOP_POLL_MILLIS);
    for _ in 0..polls {
        groups.retain(|(group, _)| test_kill_process_group(*group).is_ok());
        if groups.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(STOP_POLL_MILLIS));
    }
    for (group, start) in &groups {
        // A pid that is a live group's id is never reused, so a group whose leader has exited
        // is still the one stopped; a leader that started at another time is a reused pid.
        if test_kill_process_group(*group).is_ok()
            && start_time(*group).is_none_or(|now| now == *start)
        {
            let _ = kill_process_group(*group, Signal::KILL);
        }
    }
    match &recovery.unaccounted {
        Unaccounted::Every => {
            log.write(
                "leaving every held claim to its lease: no agent from before is accounted for",
            );
        }
        Unaccounted::Tasks(tasks) => {
            for task in tasks {
                let line = format!(
                    "leaving #{} to its lease: an agent from before may still run",
                    task.0
                );
                log.write(&line);
            }
        }
    }
    clear_agents(home, &recovery.unaccounted, timing.lease_millis, log);
    assert!(groups.len() <= LEFTOVERS_MAX);
    recovery.unaccounted
}

// Removes the agents file and every pid, left and ack file, which name no agent of this daemon,
// but keeps the pid file of an unaccounted task as its left file, apart from the pid file a new
// attempt writes, which the next start reads while that agent may live.
fn clear_agents(home: &Path, unaccounted: &Unaccounted, lease_millis: u64, log: &Log) {
    assert!(lease_millis > 0);
    let lease = Duration::from_millis(lease_millis);
    let agents = home.join(AGENTS_FILE);
    if let Err(error) = fs::remove_file(&agents)
        && error.kind() != io::ErrorKind::NotFound
    {
        log.write(&format!("could not remove {}: {error}", agents.display()));
    }
    let Ok(entries) = fs::read_dir(home.join(AGENTS_DIR)) else {
        return;
    };
    for entry in entries.take(AGENT_FILES_MAX).flatten() {
        let path = entry.path();
        let task = path
            .file_stem()
            .and_then(|stem| stem.to_str()?.parse::<u64>().ok());
        let kept = match unaccounted {
            Unaccounted::Every => true,
            Unaccounted::Tasks(tasks) => task.is_some_and(|task| tasks.contains(&TaskId(task))),
        };
        let extension = path.extension().and_then(|extension| extension.to_str());
        let lapsed = || {
            let age = entry.metadata().and_then(|meta| meta.modified()).ok();
            let age = age.and_then(|modified| modified.elapsed().ok());
            age.is_some_and(|age| age > lease) && agent_group(&path).is_none()
        };
        let removed = match extension {
            Some("ack" | STOP_EXTENSION) => fs::remove_file(&path),
            Some("pid" | LEFT_EXTENSION) if task.is_none() => {
                log.write(&format!("removing {}: it names no task", path.display()));
                fs::remove_file(&path)
            }
            Some("pid" | LEFT_EXTENSION) if !kept => fs::remove_file(&path),
            Some(LEFT_EXTENSION) if lapsed() => {
                let shown = path.display();
                log.write(&format!(
                    "removing {shown}: it names no group and its lease lapsed"
                ));
                fs::remove_file(&path)
            }
            Some("pid") if task.is_some() => fs::rename(&path, path.with_extension(LEFT_EXTENSION)),
            _ => Ok(()),
        };
        if let Err(error) = removed {
            log.write(&format!("could not clear {}: {error}", path.display()));
        }
    }
}

fn start_recorder(home: PathBuf) -> Result<SyncSender<Recorded>, DaemonError> {
    assert!(home.is_absolute());
    let (recorder, told) = sync_channel::<Recorded>(RECORDER_QUEUE_MAX);
    let builder = thread::Builder::new().name("recorder".into());
    let spawned = builder.spawn(move || record_agents(&home, &told));
    spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    Ok(recorder)
}

// Keeps `agents.json` naming the group of each running agent, as its pid file names it, until
// the daemon drops its recorder; acknowledges each group once a written `agents.json` names it.
fn record_agents(home: &Path, told: &Receiver<Recorded>) {
    let mut running: Vec<(TaskId, Option<WireAgent>)> = Vec::new();
    let mut written: Vec<WireAgent> = Vec::new();
    let mut acked: Vec<WireAgent> = Vec::new();
    let poll = Duration::from_millis(RECORD_POLL_MILLIS);
    for _ in 0..RECORDER_ROUNDS_MAX {
        match told.recv_timeout(poll) {
            Ok(Recorded::Started(task)) => {
                running.retain(|(id, _)| *id != task);
                if running.len() < AGENT_RECORDS_MAX {
                    running.push((task, None));
                }
            }
            Ok(Recorded::Ended(task)) => running.retain(|(id, _)| *id != task),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        for (task, record) in running.iter_mut() {
            let group = agent_group(&pid_file(home, *task));
            let raw = group.map(|group| group.as_raw_nonzero().get());
            if raw != record.map(|record| record.group) {
                *record = group.zip(raw).and_then(|(group, raw)| {
                    let start = start_time(group)?;
                    Some(WireAgent {
                        task: task.0,
                        group: raw,
                        start,
                    })
                });
            }
        }
        let now: Vec<WireAgent> = running.iter().filter_map(|(_, record)| *record).collect();
        if now != written {
            let file = AgentsFile {
                agents: now.clone(),
            };
            let text = serde_json::to_string(&file).unwrap_or_default();
            if write_atomically(&home.join(AGENTS_FILE), text.as_bytes()).is_ok() {
                written = now;
            }
        }
        acked.retain(|record| written.contains(record));
        for record in &written {
            if acked.contains(record) {
                continue;
            }
            let ack = ack_file(&pid_file(home, TaskId(record.task)));
            let text = format!("{} {}\n", record.group, record.start);
            if write_atomically(&ack, text.as_bytes()).is_ok() {
                acked.push(*record);
            }
        }
        assert!(running.len() <= AGENT_RECORDS_MAX);
        assert!(acked.len() <= written.len());
    }
}
