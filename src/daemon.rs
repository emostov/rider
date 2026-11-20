//! `rider daemon`: the core driven by the board's feed, a timer, signals and worker threads.

use std::cell::Cell;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::{FileExt, TryLockError};
use rustix::process::{Pid, Signal, kill_process_group};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::board::{
    Board, BoardError, Listing, TaskId, TaskView, Timestamp, VbCli, one_line, parse_failure_reason,
};
use crate::cli::with_vb_on_path;
use crate::config::Limits;
use crate::core::{
    self, ACTIONS_MAX, ATTEMPT_MILLIS_MAX, Action, Call, Config, Event, LEASE_MILLIS, RunId,
    Settlement, State, Stopping,
};
use crate::feed::{Cursor, FeedError, FeedEvent, FeedItem, FeedReader, FeedStop};
use crate::harness::{CommandLine, Exit, HarnessError, KILL_GRACE_SECONDS};
use crate::roles::{AGENTS_MAX, RoleSpec};
use crate::run_task::{
    Attempt, Env, Judgement, Prepared, RunError, judge_attempt, launch_agent, prepare_attempt,
    settle_attempt,
};
use crate::settle::Verdict;

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
/// The test-only setting that shortens limits: `lease=<ms>,attempt=<ms>,tick=<ms>,grace=<ms>`,
/// any subset.
pub const TEST_MILLIS_VAR: &str = "RIDER_TEST_MILLIS";
const TEST_SETTINGS_MAX: usize = 4;
const GRACE_MILLIS: u64 = KILL_GRACE_SECONDS * 1000;
const STOP_POLL_MILLIS: u64 = 50;
/// How long the daemon waits on exit for the feed thread once its group is killed: past the
/// feed's own grace for its stdout to close after a kill.
const FEED_JOIN_MILLIS_MAX: u64 = 15_000;
const _: () = assert!(FEED_JOIN_MILLIS_MAX >= STOP_POLL_MILLIS);
const _: () = assert!(WORKERS_MAX > AGENTS_MAX as usize && TICK_SECONDS > 0);
const NO_CHANNEL: &str = "channel_not_found";
const LOCK_FILE: &str = "daemon.lock";
const LOG_FILE: &str = "daemon.log";
const AGENTS_DIR: &str = "agents";
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
// Writes its own pid, which is its process group, then becomes the agent.
const PID_WRAPPER: &str = r#"echo $$ > "$RIDER_AGENT_PID_FILE" && exec "$@""#;

/// Where the daemon's files are and how it reaches the board and its agents.
pub struct Paths {
    pub home: PathBuf,
    pub prompts: PathBuf,
    pub vb: PathBuf,
    pub board_url: String,
    pub inherited_path: Option<String>,
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
}

impl Timing {
    fn parse(setting: Option<&str>) -> Result<Timing, DaemonError> {
        let mut timing = Timing {
            lease_millis: LEASE_MILLIS,
            attempt_millis_max: ATTEMPT_MILLIS_MAX,
            tick_millis: TICK_SECONDS * 1000,
            grace_millis: GRACE_MILLIS,
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
    attempts: u8,
    feed: Mutex<FeedSlot>,
    /// The core has decided to stop now; a settlement checks it before each board write.
    stop_now: AtomicBool,
    log: Log,
}

/// The current feed thread and how to end it.
#[derive(Default)]
struct FeedSlot {
    /// The daemon is exiting: no feed starts and none reports its loss.
    stopping: bool,
    stop: Option<FeedStop>,
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
            prompts: self.paths.prompts.clone(),
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

    fn pid_file(&self, task: TaskId) -> PathBuf {
        let path = self
            .paths
            .home
            .join(AGENTS_DIR)
            .join(format!("{}.pid", task.0));
        assert!(path.starts_with(&self.paths.home));
        path
    }
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
    let shared = Arc::new(Shared {
        paths,
        events,
        lease_millis,
        grace_millis: timing.grace_millis,
        attempts: limits.attempts,
        feed: Mutex::new(FeedSlot::default()),
        stop_now: AtomicBool::new(false),
        log,
    });
    start_workers(queued, &shared.events)?;
    start_timer(shared.events.clone(), timing.tick_millis)?;
    start_signals(shared.events.clone())?;
    let mut config = Config::from_roles(roles);
    config.agents_max = limits.agents;
    config.reconcile_millis = RECONCILE_SECONDS * 1000;
    config.lease_millis = lease_millis;
    config.attempt_millis_max = timing.attempt_millis_max.min(limits.attempt_millis);
    let mut state = State::new(config, now());
    let code = drive(&mut state, &inputs, &jobs, &shared);
    stop_feed(&shared);
    let code = code?;
    assert!(state.exited());
    Ok(code)
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
    let tick = step(state, Event::Tick(now()));
    let mut pending: Vec<Action> = tick;
    for _ in 0..STEPS_MAX {
        for action in pending.drain(..) {
            if let Action::Exit(code) = action {
                return Ok(u8::try_from(code).unwrap_or(1));
            }
            if let Some(job) = dispatch(action, &mut contexts, shared) {
                jobs.try_send(job).map_err(|error| match error {
                    TrySendError::Full(_) => DaemonError::JobsFull,
                    TrySendError::Disconnected(_) => DaemonError::QueueClosed,
                })?;
            }
        }
        let input = inputs.recv().map_err(|_| DaemonError::QueueClosed)?;
        if !following && let Event::Snapshot { cursor, .. } = &input.event {
            start_feed(shared, *cursor)?;
            following = true;
            println!("{FOLLOWING} {SUBTREE} on {}", shared.paths.board_url);
        }
        let event = absorb(input, &mut contexts, &shared.log);
        pending = step(state, event);
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
    if let Action::Claim { run, task, profile } = &action {
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
        Action::Prepare { run, task } => {
            let profile = find(contexts, run, lost(run, "prepare"))?.profile.clone();
            Box::new(move || prepare(&shared, run, task, &profile))
        }
        Action::Spawn { run, task, profile } => {
            let context = find(contexts, run, Event::AgentExited(run))?;
            let attempt = context.attempt.clone().ok_or(Event::AgentExited(run))?;
            shared.log.write(&format!("spawned #{} {profile}", task.0));
            Box::new(move || spawn(&shared, run, task, &attempt))
        }
        Action::StopAgent { run, task } => Box::new(move || stop_agent(&shared, run, task)),
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
        Action::Exit(_) => return Ok(None),
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
        BoardError::Spawn(_) | BoardError::Io(_) | BoardError::TimedOut { .. }
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

fn prepare(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    let board = shared.board(profile);
    let prepared = prepare_attempt(task, &shared.env(profile), &board);
    match prepared {
        Ok(Prepared::Ready(attempt)) => {
            let branch = attempt.branch().map(str::to_string);
            let attempt = *attempt;
            Input {
                event: Event::Prepared { run, branch },
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

fn spawn(shared: &Shared, run: RunId, task: TaskId, attempt: &Attempt) -> Input {
    let pid_file = shared.pid_file(task);
    let vb_dir = shared
        .paths
        .vb
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned());
    let vb_dir = vb_dir.unwrap_or_else(|| "/".to_string());
    let inherited = shared.paths.inherited_path.clone();
    let pid_text = pid_file.to_string_lossy().into_owned();
    let launch = move |cmd: &CommandLine, dir: &Path, log: &Path| {
        let cmd = with_vb_on_path(cmd, &vb_dir, inherited.as_deref());
        let mut args = vec!["-c".to_string(), PID_WRAPPER.to_string(), "sh".to_string()];
        args.push(cmd.program.clone());
        args.extend(cmd.args.iter().cloned());
        let mut env = cmd.env.clone();
        env.push(("RIDER_AGENT_PID_FILE".to_string(), pid_text.clone()));
        let wrapped = CommandLine {
            program: "/bin/sh".to_string(),
            args,
            env,
        };
        launch_agent(&wrapped, dir, log)
    };
    let launched = attempt.launch(&launch);
    // The agent is gone, so its pid may be reused: nothing may signal it any more.
    let _ = fs::remove_file(&pid_file);
    Input {
        event: Event::AgentExited(run),
        extra: Extra::Launched(launched),
    }
}

fn stop_agent(shared: &Shared, run: RunId, task: TaskId) -> Input {
    assert!(shared.grace_millis > 0 && shared.grace_millis <= GRACE_MILLIS);
    let path = shared.pid_file(task);
    if let Some(group) = agent_group(&path) {
        // A group that is already gone has nothing left to stop.
        let _ = kill_process_group(group, Signal::TERM);
        let polls = shared.grace_millis.div_ceil(STOP_POLL_MILLIS);
        for _ in 0..polls {
            thread::sleep(Duration::from_millis(STOP_POLL_MILLIS));
            // The pid file goes only once the agent is reaped.
            if agent_group(&path) != Some(group) {
                return Input::plain(Event::StopDone(run));
            }
        }
        let _ = kill_process_group(group, Signal::KILL);
    }
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
            let judgement = Judgement::uninspected(verdict.clone());
            settle_attempt(task, &shared.env(profile), &board, attempt, &judgement)
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
            Event::ClaimLost(run)
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
}

fn release(shared: &Shared, run: RunId, task: TaskId, profile: &str) -> Input {
    let event = match shared.board(profile).release(task) {
        Ok(()) => Event::Settled(run),
        Err(BoardError::ClaimLost { .. }) => Event::ClaimLost(run),
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
