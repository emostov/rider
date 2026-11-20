//! `rider daemon`: the core driven by the board's feed, a timer, signals and worker threads.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::board::{Board, BoardError, TaskId, Timestamp, VbCli, one_line, parse_failure_reason};
use crate::cli::with_vb_on_path;
use crate::core::{
    self, ACTIONS_MAX, ATTEMPT_MILLIS_MAX, Action, Call, Config, Event, LEASE_MILLIS, RunId,
    Settlement, State,
};
use crate::feed::{Cursor, FeedError, FeedEvent, FeedItem, FeedReader};
use crate::harness::{CommandLine, Exit, HarnessError, KILL_GRACE_SECONDS};
use crate::roles::{AGENTS_MAX, ROLES};
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
    /// Another live process holds the lock.
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

/// The daemon lock under the Rider home, removed when dropped.
pub struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // A lock that is already gone needs no removing.
        let _ = fs::remove_file(&self.path);
    }
}

/// Takes `daemon.lock` under `home`, taking over a lock whose process is gone.
pub fn take_lock(home: &Path) -> Result<Lock, DaemonError> {
    assert!(home.is_absolute());
    fs::create_dir_all(home).map_err(io_error("create", home))?;
    let path = home.join(LOCK_FILE);
    let pid = std::process::id();
    let text = format!("pid {pid} started {}\n", clock_line(SystemTime::now()));
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .map_err(io_error("write", &path))?;
                assert!(path.starts_with(home));
                return Ok(Lock { path });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if let Some(holder) = live_holder(&path)? {
                    return Err(DaemonError::Held { path, holder });
                }
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error("remove", &path)(error)),
                }
            }
            Err(error) => return Err(io_error("create", &path)(error)),
        }
    }
    let holder = "a process that took it while it was being taken over".to_string();
    Err(DaemonError::Held { path, holder })
}

// The holder named in a lock file, when its process still exists; a malformed lock is held.
fn live_holder(path: &Path) -> Result<Option<String>, DaemonError> {
    let mut text = String::new();
    let file = File::open(path).map_err(io_error("open", path))?;
    let read = file.take(LOCK_BYTES_MAX).read_to_string(&mut text);
    read.map_err(io_error("read", path))?;
    let text = one_line(text.trim());
    let pid = text
        .strip_prefix("pid ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|pid| pid.parse::<i32>().ok())
        .and_then(Pid::from_raw);
    let Some(pid) = pid else {
        return Ok(Some(format!(
            "an unreadable lock {text:?}; remove it if no rider runs"
        )));
    };
    assert!(pid.as_raw_nonzero().get() > 0);
    let alive = match test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    };
    Ok(alive.then_some(text))
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
pub fn run(paths: Paths) -> Result<u8, DaemonError> {
    assert!(paths.home.is_absolute() && paths.vb.is_absolute());
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
    let shared = Arc::new(Shared {
        paths,
        events,
        lease_millis,
        grace_millis: timing.grace_millis,
    });
    start_workers(queued, &shared.events)?;
    start_timer(shared.events.clone(), timing.tick_millis)?;
    start_signals(shared.events.clone())?;
    let mut config = Config::from_roles(&ROLES);
    config.reconcile_millis = RECONCILE_SECONDS * 1000;
    config.lease_millis = lease_millis;
    config.attempt_millis_max = timing.attempt_millis_max;
    let mut state = State::new(config, now());
    let code = drive(&mut state, &inputs, &jobs, &shared)?;
    assert!(state.exited());
    Ok(code)
}

fn drive(
    state: &mut State,
    inputs: &Receiver<Input>,
    jobs: &SyncSender<Job>,
    shared: &Arc<Shared>,
) -> Result<u8, DaemonError> {
    let mut contexts: Vec<RunContext> = Vec::with_capacity(usize::from(AGENTS_MAX));
    let mut following = false;
    let log = shared.paths.home.join(LOG_FILE);
    let tick = step(state, Event::Tick(now()));
    let mut pending: Vec<Action> = tick;
    for _ in 0..STEPS_MAX {
        for action in pending.drain(..) {
            if let Action::Exit(code) = action {
                return Ok(u8::try_from(code).unwrap_or(1));
            }
            let job = dispatch(action, &mut contexts, shared, &log);
            if let Some(job) = job {
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
        let event = absorb(input, &mut contexts, &log);
        pending = step(state, event);
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
fn absorb(input: Input, contexts: &mut [RunContext], log: &Path) -> Event {
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
                write_log(log, &line);
            }
            (Event::Settled(_), _) => {
                write_log(
                    log,
                    &format!("settled #{} {}", context.task.0, context.profile),
                );
            }
            (_, Extra::Attempt(attempt)) => context.attempt = Some(attempt),
            (_, Extra::Launched(launched)) => context.launched = Some(launched),
            (_, Extra::Judgement(judgement)) => context.judgement = Some(judgement),
            (_, Extra::None) => {}
        }
    }
    input.event
}

fn dispatch(
    action: Action,
    contexts: &mut Vec<RunContext>,
    shared: &Arc<Shared>,
    log: &Path,
) -> Option<Job> {
    assert!(contexts.len() <= usize::from(AGENTS_MAX));
    let shared = Arc::clone(shared);
    let find = |contexts: &[RunContext], run: RunId| {
        contexts.iter().position(|context| context.run == run)
    };
    let job: Job = match action {
        Action::Claim { run, task, profile } => {
            contexts.push(RunContext::new(run, task, profile.clone()));
            Box::new(move || claim(&shared, run, task, &profile))
        }
        Action::Prepare { run, task } => {
            let index = find(contexts.as_slice(), run)?;
            let profile = contexts[index].profile.clone();
            Box::new(move || prepare(&shared, run, task, &profile))
        }
        Action::Spawn { run, task, profile } => {
            let index = find(contexts.as_slice(), run)?;
            let attempt = contexts[index].attempt.clone()?;
            write_log(log, &format!("spawned #{} {profile}", task.0));
            Box::new(move || spawn(&shared, run, task, &attempt))
        }
        Action::StopAgent { run, task } => Box::new(move || stop_agent(&shared, run, task)),
        Action::Judge { run, task } => {
            let index = find(contexts.as_slice(), run)?;
            let context = &mut contexts[index];
            let (profile, attempt) = (context.profile.clone(), context.attempt.clone()?);
            let launched = context.launched.take()?;
            Box::new(move || judge(&shared, run, task, &profile, &attempt, launched))
        }
        Action::Renew { run, task } => {
            let index = find(contexts.as_slice(), run)?;
            let profile = contexts[index].profile.clone();
            Box::new(move || renew(&shared, run, task, &profile))
        }
        Action::Settle {
            run,
            task,
            settlement,
            ..
        } => {
            let index = find(contexts.as_slice(), run)?;
            let context = &contexts[index];
            let profile = context.profile.clone();
            let (attempt, judgement) = (context.attempt.clone(), context.judgement.clone());
            let close = Close {
                settlement,
                attempt,
                judgement,
            };
            Box::new(move || settle(&shared, run, task, &profile, close))
        }
        Action::Release { run, task, .. } => {
            let index = find(contexts.as_slice(), run)?;
            let profile = contexts[index].profile.clone();
            Box::new(move || release(&shared, run, task, &profile))
        }
        Action::ListTasks => Box::new(move || list(&shared)),
        Action::RestartFeed(cursor) => Box::new(move || restart_feed(&shared, cursor)),
        Action::Exit(_) => return None,
    };
    Some(job)
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
    let board = shared.board(profile);
    let written = match (&close.settlement, &close.attempt, &close.judgement) {
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

// Starts a thread that follows the feed after `cursor` and reports its loss once.
fn start_feed(shared: &Arc<Shared>, cursor: Cursor) -> Result<(), DaemonError> {
    let board = shared.board(DAEMON_PROFILE);
    let events = shared.events.clone();
    let builder = thread::Builder::new().name("feed".into());
    let spawned = builder.spawn(move || {
        let mut reader = match FeedReader::start(&board, cursor) {
            Ok(reader) => reader,
            Err(_) => return send(&events, Event::FeedLost),
        };
        for _ in 0..FEED_ITEMS_MAX {
            match reader.next_item() {
                Ok(FeedItem::Event(FeedEvent::Known(event))) => {
                    if event.channel.starts_with(SUBTREE) && !send(&events, Event::Feed(event)) {
                        return false;
                    }
                }
                Ok(FeedItem::Event(FeedEvent::Other { .. }) | FeedItem::Idle) => {}
                Err(FeedError::LineTooLong | FeedError::Parse(_) | FeedError::TooManyLabels) => {}
                Ok(FeedItem::Exited { .. }) | Err(_) => break,
            }
        }
        send(&events, Event::FeedLost)
    });
    spawned.map_err(|error| DaemonError::Thread(error.to_string()))?;
    Ok(())
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

// A timestamped line appended to the daemon's log; losing one must not stop the daemon.
fn write_log(path: &Path, line: &str) {
    assert!(!line.contains('\n'));
    let text = format!("{} {line}\n", clock_line(SystemTime::now()));
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
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
