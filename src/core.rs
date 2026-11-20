//! The daemon's decisions for workers: one pure `step` from a state and an event to actions.

use crate::board::{BOARD_CALL_SECONDS_MAX, LABELS_MAX, ListedTask, TaskId, TaskState, Timestamp};
use crate::feed::{BoardEvent, Cursor, EventKind};
use crate::roles::{AGENTS_MAX, BUILDERS_DEFAULT, RoleSpec};
use crate::settle::Verdict;

/// Upper bound on the open tasks the core tracks.
pub const TASKS_MAX: usize = 256;
/// Upper bound on the roles in the table the core is given.
pub const ROLES_MAX: usize = 16;
/// Upper bound on the socket clients connected at once.
pub const CLIENTS_MAX: usize = 8;
/// Upper bound on the actions one step emits: a few per run, a line per ended run for each
/// client waiting on `down`, an end for each client following a log, plus a few of its own.
pub const ACTIONS_MAX: usize =
    6 * AGENTS_MAX as usize + CLIENTS_MAX * (AGENTS_MAX as usize + 1) + 2 * CLIENTS_MAX + 8;
/// How long a claim holds its task, in milliseconds.
pub const LEASE_MILLIS: u64 = 3600 * 1000;
/// How long one attempt may run, in milliseconds.
pub const ATTEMPT_MILLIS_MAX: u64 = 55 * 60 * 1000;
/// How many times the closing write of a run is tried.
pub const SETTLE_TRIES_MAX: u8 = 5;
/// The pause between tries of a closing write, in milliseconds.
pub const SETTLE_PAUSE_MILLIS: u64 = 10 * 1000;
/// How often a snapshot is taken, in milliseconds.
pub const RECONCILE_MILLIS: u64 = 60 * 1000;
/// How long a drain may last before it becomes a stop now, in milliseconds.
pub const DRAIN_MILLIS_MAX: u64 = ATTEMPT_MILLIS_MAX + 5 * 60 * 1000;
/// How long a claim may wait for its answer, in milliseconds.
pub const CLAIM_MILLIS_MAX: u64 = 2 * BOARD_CALL_SECONDS_MAX as u64 * 1000;
/// How long preparing a run may take, in milliseconds.
pub const PREPARE_MILLIS_MAX: u64 = 10 * 60 * 1000;
/// How long judging a run may take, in milliseconds.
pub const JUDGE_MILLIS_MAX: u64 = 10 * 60 * 1000;
/// How long a stopped agent may take to exit, in milliseconds.
pub const STOP_MILLIS_MAX: u64 = 2 * 60 * 1000;
/// How many times a stop is sent before the agent counts as lost.
pub const STOP_TRIES_MAX: u8 = 3;
/// Upper bound on claims given up while the board may still hold them: claims in flight are
/// kept below `AGENTS_MAX` together with these, and each run may join them.
pub const PENDING_CLAIMS_MAX: usize = 2 * AGENTS_MAX as usize;
/// Upper bound on the distinct untracked tasks the core counts.
pub const UNTRACKED_MAX: usize = 4 * TASKS_MAX;
/// Upper bound on the closed tasks the core remembers until a snapshot covers their closing.
pub const CLOSED_MAX: usize = TASKS_MAX;
/// How many feed restarts in a row are allowed before the board counts as down.
pub const FEED_RESTARTS_MAX: u8 = 10;
/// How long a feed must stay up, without delivering an event, for its loss not to count as
/// a restart in a row, in milliseconds.
pub const FEED_UPTIME_MILLIS_MIN: u64 = 60 * 1000;

/// The failure reason of a task whose agent the owner killed.
pub const KILLED_REASON: &str = "killed by the owner";

/// One role as the core sees it: its label, its profile's model part, its slots, and whether
/// its runs count against the builders limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub label: String,
    pub model_slug: String,
    pub slots: u8,
    pub builds: bool,
}

/// The role table and limits the core runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub roles: Vec<Role>,
    pub agents_max: u8,
    /// Runs of building roles at once.
    pub builders_max: u8,
    pub tasks_max: usize,
    pub lease_millis: u64,
    pub attempt_millis_max: u64,
    pub settle_tries_max: u8,
    pub settle_pause_millis: u64,
    pub reconcile_millis: u64,
    pub drain_millis_max: u64,
    pub claim_millis_max: u64,
    pub prepare_millis_max: u64,
    pub judge_millis_max: u64,
    pub stop_millis_max: u64,
}

impl Config {
    /// The configuration built from a role table and the default limits.
    pub fn from_roles(specs: &[RoleSpec]) -> Config {
        assert!(!specs.is_empty());
        assert!(specs.len() <= ROLES_MAX);
        let roles = specs
            .iter()
            .map(|spec| Role {
                label: spec.label.to_string(),
                model_slug: spec.model_slug.to_string(),
                slots: spec.slots,
                builds: spec.builds,
            })
            .collect();
        Config {
            roles,
            agents_max: AGENTS_MAX,
            builders_max: BUILDERS_DEFAULT,
            tasks_max: TASKS_MAX,
            lease_millis: LEASE_MILLIS,
            attempt_millis_max: ATTEMPT_MILLIS_MAX,
            settle_tries_max: SETTLE_TRIES_MAX,
            settle_pause_millis: SETTLE_PAUSE_MILLIS,
            reconcile_millis: RECONCILE_MILLIS,
            drain_millis_max: DRAIN_MILLIS_MAX,
            claim_millis_max: CLAIM_MILLIS_MAX,
            prepare_millis_max: PREPARE_MILLIS_MAX,
            judge_millis_max: JUDGE_MILLIS_MAX,
            stop_millis_max: STOP_MILLIS_MAX,
        }
    }

    /// How long a run may stay in `phase`; a closing write gets at most one lease.
    pub fn phase_millis_max(&self, phase: Phase) -> u64 {
        let limit = match phase {
            Phase::Claiming => self.claim_millis_max,
            Phase::Preparing => self.prepare_millis_max,
            Phase::Running => self.attempt_millis_max,
            Phase::Judging => self.judge_millis_max,
            Phase::Settling => self.lease_millis,
            Phase::Stopping(_) | Phase::Abandoning => self.stop_millis_max,
        };
        assert!(limit > 0);
        limit
    }

    fn assert_valid(&self) {
        assert!(!self.roles.is_empty() && self.roles.len() <= ROLES_MAX);
        assert!(self.roles.iter().all(|role| role.slots > 0));
        assert!(self.agents_max > 0 && self.agents_max <= AGENTS_MAX);
        assert!(self.builders_max > 0 && self.builders_max <= AGENTS_MAX);
        assert!(self.tasks_max > 0 && self.tasks_max <= TASKS_MAX);
        assert!(self.lease_millis >= 3 && self.attempt_millis_max > 0);
        assert!(self.settle_tries_max > 0);
        assert!(self.claim_millis_max > 0 && self.prepare_millis_max > 0);
        assert!(self.judge_millis_max > 0 && self.stop_millis_max > 0);
    }
}

/// The core's identity for one run, so a late result for an earlier run is told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(pub u64);

/// An open task as the core last saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    /// Index into the config's roles; `None` when the task has no known role.
    pub role: Option<usize>,
    pub branch: Option<String>,
    /// The project the task's channel names under `work/`, if it names one.
    pub project: Option<String>,
    /// The task's board version: the cursor of the newest event this was learned from.
    pub seen: Cursor,
    /// A claim of the task was refused since it was last learned or the last tick.
    pub claim_refused: bool,
}

/// Why a running agent is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    TimeLimit,
    StopNow,
    /// The owner asked for it with `rider kill`.
    Killed,
}

/// Where a run is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Claiming,
    Preparing,
    Running,
    Judging,
    Settling,
    Stopping(StopReason),
    /// The claim was lost; the agent is being stopped and nothing is written.
    Abandoning,
}

/// The closing write of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    Verdict(Verdict),
    Block { kind: String, reason: String },
    Release,
}

/// One task the daemon is working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: RunId,
    pub task: TaskId,
    pub role: usize,
    /// Counts from 1, at most the role's slots.
    pub slot: u8,
    pub phase: Phase,
    pub epoch: u64,
    pub lease_until: Timestamp,
    pub attempt_start: Timestamp,
    pub phase_start: Timestamp,
    pub renewing: bool,
    /// The closing write, once decided; it is retried until it succeeds or runs out.
    pub settlement: Option<Settlement>,
    pub settle_tries: u8,
    /// A closing write is in flight.
    pub settling: bool,
    pub retry_at: Timestamp,
    /// The agent process exists and has not been seen to exit.
    pub agent_alive: bool,
    /// The branch this run reserves, from its claim or its preparation until it is removed.
    pub branch: Option<String>,
    /// How many stops were sent to the agent in this phase.
    pub stop_tries: u8,
    /// Every stop went unanswered: the run, its slot and its claim stay until the agent exits.
    pub agent_lost: bool,
}

/// A claim given up while the board may hold it: once its epoch is known, it is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClaim {
    pub run: RunId,
    pub task: TaskId,
    /// The profile the claim was sent as, which a release of a late grant writes as.
    pub profile: String,
    /// When any grant of the claim has lapsed.
    pub until: Timestamp,
    /// The granted epoch, once known.
    pub epoch: Option<u64>,
    pub releasing: bool,
    pub retry_at: Timestamp,
}

/// How many open tasks the core saw but does not track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Untracked {
    Exactly(usize),
    /// More were seen than `UNTRACKED_MAX` lets the core tell apart.
    AtLeast(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardHealth {
    Up,
    Down { since: Timestamp },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopping {
    No,
    Drain { since: Timestamp },
    Now,
}

/// A socket connection, as the socket names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u64);

/// What a socket client asks of the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Status,
    Agents,
    /// Stop: drain, or with `now` stop every agent at once.
    Down {
        now: bool,
    },
    /// Stop the task's agent and fail the task; the attempt counts.
    Kill(TaskId),
    /// The newest attempt's log of the task; with `follow`, until its agent exits.
    Logs {
        task: TaskId,
        follow: bool,
    },
}

/// One open task as `status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: TaskId,
    /// The project the task's channel names under `work/`, if it names one.
    pub project: Option<String>,
    pub state: TaskState,
    pub role: Option<String>,
    /// The profile of the run working it, if any.
    pub profile: Option<String>,
    /// How long the run working it has run its attempt, in milliseconds.
    pub age_millis: Option<u64>,
    pub branch: Option<String>,
}

/// What `status` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusView {
    pub board: BoardHealth,
    pub stopping: Stopping,
    pub running: usize,
    pub limit: u8,
    pub untracked: Untracked,
    pub tasks: Vec<TaskRow>,
}

/// One run as `agents` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub task: TaskId,
    pub role: String,
    pub profile: String,
    pub phase: Phase,
    pub age_millis: u64,
}

/// One message to a socket client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Status(StatusView),
    Agents(Vec<AgentRow>),
    /// A streaming command was accepted; lines follow.
    Accepted,
    /// One line of a stream.
    Line(String),
    /// The stream ended.
    End,
    /// The request was carried out; nothing follows.
    Done,
    /// Stream the task's newest log; with `follow`, keep streaming until `End` follows.
    Log {
        task: TaskId,
        follow: bool,
    },
    /// The request was refused with an error code.
    Refused {
        code: ErrorCode,
        message: String,
    },
}

/// Why the core refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    UnknownTask,
    NotRunning,
}

/// The board call a `BoardFailed` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Claim(RunId),
    Renew(RunId),
    Settle(RunId),
    Release(RunId),
    ListTasks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Tick(Timestamp),
    Feed(BoardEvent),
    FeedLost,
    FeedResumed(Cursor),
    /// A listing: its tasks, the cursor read before its first page, and how many it left out.
    Snapshot {
        tasks: Vec<ListedTask>,
        cursor: Cursor,
        left_out: usize,
    },
    Claimed {
        run: RunId,
        epoch: u64,
        until: Timestamp,
    },
    ClaimRefused(RunId),
    /// The worktree is ready; `branch` is the branch preparation found, if any, and `role` the
    /// label of the role the agent would launch as.
    Prepared {
        run: RunId,
        branch: Option<String>,
        role: String,
    },
    PrepareBlocked {
        run: RunId,
        kind: String,
        reason: String,
    },
    AgentExited(RunId),
    /// A `StopAgent` was carried out; the agent's exit is reported separately.
    StopDone(RunId),
    Judged {
        run: RunId,
        verdict: Verdict,
    },
    Renewed {
        run: RunId,
        until: Timestamp,
    },
    /// A renewal or fenced write was refused because the claim is no longer held.
    ClaimLost(RunId),
    /// A closing write, `Settle` or `Release`, succeeded.
    Settled(RunId),
    /// A board call timed out or could not connect.
    BoardFailed(Call),
    /// SIGTERM or SIGINT: the first drains, the next stops now.
    Signal,
    /// A socket client's request.
    Client {
        client: ClientId,
        request: Request,
    },
    /// A socket client disconnected.
    ClientGone(ClientId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Claim {
        run: RunId,
        task: TaskId,
        profile: String,
    },
    Prepare {
        run: RunId,
        task: TaskId,
    },
    Spawn {
        run: RunId,
        task: TaskId,
        profile: String,
    },
    Renew {
        run: RunId,
        task: TaskId,
    },
    StopAgent {
        run: RunId,
        task: TaskId,
    },
    Judge {
        run: RunId,
        task: TaskId,
    },
    Settle {
        run: RunId,
        task: TaskId,
        epoch: u64,
        settlement: Settlement,
    },
    Release {
        run: RunId,
        task: TaskId,
        epoch: u64,
        profile: String,
    },
    ListTasks,
    RestartFeed(Cursor),
    Reply {
        client: ClientId,
        reply: Reply,
    },
    Exit(i32),
}

/// Everything the core knows; changed only by `step`.
#[derive(Debug, Clone)]
pub struct State {
    config: Config,
    tasks: Vec<Task>,
    /// Each untracked task with the cursor it was last seen at.
    untracked: Vec<(TaskId, Cursor)>,
    /// The newest cursor at which an untracked task went uncounted.
    untracked_overflow: Option<Cursor>,
    /// When the last listing left tasks out: a lower bound on the untracked tasks.
    left_out: Option<usize>,
    /// Tasks the feed closed, with the closing event's cursor, until a snapshot covers it.
    closed: Vec<(TaskId, Cursor)>,
    /// The newest closing cursor that did not fit in `closed`.
    closed_unrecorded: Cursor,
    runs: Vec<Run>,
    pending_claims: Vec<PendingClaim>,
    next_run: u64,
    board: BoardHealth,
    stopping: Stopping,
    cursor: Cursor,
    now: Timestamp,
    last_reconcile: Timestamp,
    listing: bool,
    feed_restarts: u8,
    feed_since: Timestamp,
    feed_delivered: bool,
    /// Clients waiting on `down`, told of each run as it ends; at most `CLIENTS_MAX`.
    down_clients: Vec<ClientId>,
    /// Clients following a task's log, told when its agent is gone; at most `CLIENTS_MAX`.
    log_clients: Vec<(ClientId, TaskId)>,
    exited: bool,
}

impl State {
    /// A fresh state at `now`; the first tick asks for a snapshot.
    pub fn new(config: Config, now: Timestamp) -> State {
        config.assert_valid();
        let state = State {
            config,
            tasks: Vec::new(),
            untracked: Vec::new(),
            untracked_overflow: None,
            left_out: None,
            closed: Vec::new(),
            closed_unrecorded: Cursor(0),
            runs: Vec::new(),
            pending_claims: Vec::new(),
            next_run: 1,
            board: BoardHealth::Up,
            stopping: Stopping::No,
            cursor: Cursor(0),
            now,
            last_reconcile: Timestamp(0),
            listing: false,
            feed_restarts: 0,
            feed_since: now,
            feed_delivered: false,
            down_clients: Vec::new(),
            log_clients: Vec::new(),
            exited: false,
        };
        assert!(state.runs.is_empty());
        state
    }

    pub fn config(&self) -> &Config {
        assert!(!self.config.roles.is_empty());
        assert!(self.config.roles.len() <= ROLES_MAX);
        &self.config
    }
    pub fn tasks(&self) -> &[Task] {
        assert!(self.tasks.len() <= self.config.tasks_max);
        let roles = self.config.roles.len();
        assert!(
            self.tasks
                .iter()
                .all(|task| task.role.is_none_or(|role| role < roles))
        );
        &self.tasks
    }
    /// Tasks seen but not tracked because `tasks_max` was reached.
    pub fn untracked(&self) -> Untracked {
        let count = self.untracked.len();
        assert!(count <= UNTRACKED_MAX);
        let tracked = |id: TaskId| self.tasks.iter().any(|task| task.id == id);
        assert!(self.untracked.iter().all(|(id, _)| !tracked(*id)));
        match (self.untracked_overflow, self.left_out) {
            (None, None) => Untracked::Exactly(count),
            (_, floor) => Untracked::AtLeast(count.max(floor.unwrap_or(0))),
        }
    }
    pub fn runs(&self) -> &[Run] {
        assert!(self.runs.len() <= usize::from(self.config.agents_max));
        let roles = &self.config.roles;
        let valid = |run: &Run| run.role < roles.len() && run.slot >= 1;
        assert!(
            self.runs
                .iter()
                .all(|run| valid(run) && run.slot <= roles[run.role].slots)
        );
        &self.runs
    }
    pub fn pending_claims(&self) -> &[PendingClaim] {
        assert!(self.pending_claims.len() <= PENDING_CLAIMS_MAX);
        let given_up = |pending: &PendingClaim| self.run_index(pending.run).is_none();
        assert!(self.pending_claims.iter().all(given_up));
        &self.pending_claims
    }
    pub fn board(&self) -> BoardHealth {
        self.board
    }
    pub fn stopping(&self) -> Stopping {
        assert!(self.stopping != Stopping::No || !self.exited);
        let running = self.runs.iter().any(|run| run.phase == Phase::Running);
        assert!(self.stopping != Stopping::Now || !running);
        self.stopping
    }
    pub fn now(&self) -> Timestamp {
        self.now
    }
    pub fn exited(&self) -> bool {
        assert!(!self.exited || self.runs.is_empty());
        assert!(!self.exited || self.pending_claims.is_empty());
        self.exited
    }

    fn run_index(&self, id: RunId) -> Option<usize> {
        let index = self.runs.iter().position(|run| run.id == id);
        assert!(index.is_none_or(|index| self.runs[index + 1..].iter().all(|run| run.id != id)));
        index
    }

    fn profile(&self, role: usize, slot: u8) -> String {
        assert!(role < self.config.roles.len());
        let role = &self.config.roles[role];
        assert!(slot >= 1 && slot <= role.slots);
        format!("{}-{slot}-{}", role.label, role.model_slug)
    }
}

/// Applies one event and returns the actions it calls for, at most `ACTIONS_MAX`.
pub fn step(state: &mut State, event: Event) -> Vec<Action> {
    assert!(state.runs.len() <= usize::from(state.config.agents_max));
    let mut actions = Vec::new();
    if state.exited {
        return actions;
    }
    let cursor = state.cursor;
    let before: Vec<(RunId, TaskId, String)> = state
        .runs
        .iter()
        .map(|run| (run.id, run.task, state.profile(run.role, run.slot)))
        .collect();
    apply(state, event, &mut actions);
    assert!(state.cursor >= cursor);
    expire_leases(state, &mut actions);
    on_time(state, &mut actions);
    expire_phases(state, &mut actions);
    claim_ready(state, &mut actions);
    tell_ended(state, &before, &mut actions);
    tell_followers(state, &mut actions);
    finish_stopping(state, &mut actions);
    assert!(actions.len() <= ACTIONS_MAX);
    assert!(state.runs.len() <= usize::from(state.config.agents_max));
    assert!(state.tasks.len() <= state.config.tasks_max);
    actions
}

fn apply(state: &mut State, event: Event, actions: &mut Vec<Action>) {
    assert!(!state.exited);
    match event {
        Event::Tick(now) => {
            state.now = now;
            (state.tasks.iter_mut()).for_each(|task| task.claim_refused = false);
            if state.board != BoardHealth::Up && !state.listing {
                state.listing = true;
                actions.push(Action::ListTasks);
            }
        }
        Event::Feed(feed) => track_feed(state, &feed),
        Event::FeedLost => feed_lost(state, actions),
        Event::FeedResumed(cursor) => {
            state.feed_since = state.now;
            state.feed_delivered = false;
            state.cursor = state.cursor.max(cursor);
        }
        Event::Snapshot {
            tasks,
            cursor,
            left_out,
        } => {
            state.listing = false;
            state.board = BoardHealth::Up;
            state.cursor = state.cursor.max(cursor);
            reconcile(state, &tasks, cursor, left_out);
        }
        Event::BoardFailed(call) => board_failed(state, call),
        Event::Signal => signal(state, actions),
        Event::Client { client, request } => on_request(state, client, request, actions),
        Event::ClientGone(client) => {
            state.down_clients.retain(|id| *id != client);
            state.log_clients.retain(|(id, _)| *id != client);
        }
        Event::Claimed { run, epoch, until } => on_claimed(state, run, epoch, until, actions),
        Event::ClaimRefused(run) => claim_refused(state, run),
        Event::Prepared { run, branch, role } => on_prepared(state, run, branch, &role, actions),
        Event::PrepareBlocked { run, kind, reason } => {
            if let Some(index) = run_in(state, run, Phase::Preparing) {
                begin_settle(state, index, Settlement::Block { kind, reason }, actions);
            }
        }
        Event::AgentExited(run) => on_agent_exited(state, run, actions),
        Event::StopDone(_) => {}
        Event::Judged { run, verdict } => {
            if let Some(index) = run_in(state, run, Phase::Judging) {
                begin_settle(state, index, Settlement::Verdict(verdict), actions);
            }
        }
        Event::Renewed { run, until } => {
            state.board = BoardHealth::Up;
            if let Some(index) = state.run_index(run) {
                let entry = &mut state.runs[index];
                entry.renewing = false;
                entry.lease_until = entry.lease_until.max(until);
            }
        }
        Event::ClaimLost(run) => {
            state.pending_claims.retain(|pending| pending.run != run);
            if let Some(index) = state.run_index(run) {
                abandon(state, index, actions);
            }
            let index = state.run_index(run);
            assert!(index.is_none_or(|index| state.runs[index].phase == Phase::Abandoning));
        }
        Event::Settled(run) => {
            state.board = BoardHealth::Up;
            state
                .pending_claims
                .retain(|pending| pending.run != run || !pending.releasing);
            if let Some(index) = state.run_index(run)
                && state.runs[index].settling
            {
                remove_run(state, index);
            }
        }
    }
}

/// A loss counts as one more in a row unless the feed delivered an event or stayed up long.
fn feed_lost(state: &mut State, actions: &mut Vec<Action>) {
    let uptime = state.now.0.saturating_sub(state.feed_since.0);
    if state.feed_delivered || uptime >= FEED_UPTIME_MILLIS_MIN {
        state.feed_restarts = 0;
    }
    state.feed_restarts = state.feed_restarts.saturating_add(1);
    if state.feed_restarts > FEED_RESTARTS_MAX {
        mark_down(state);
    }
    assert!(state.feed_restarts <= FEED_RESTARTS_MAX || state.board != BoardHealth::Up);
    actions.push(Action::RestartFeed(state.cursor));
    if !state.listing {
        state.listing = true;
        actions.push(Action::ListTasks);
    }
    assert!(state.listing);
}

fn on_request(state: &mut State, client: ClientId, request: Request, actions: &mut Vec<Action>) {
    let reply = match request {
        Request::Status => Reply::Status(status_view(state)),
        Request::Agents => Reply::Agents(agent_rows(state)),
        Request::Down { now } => {
            if !state.down_clients.contains(&client) && state.down_clients.len() < CLIENTS_MAX {
                state.down_clients.push(client);
            }
            let stopping = match (state.stopping, now) {
                (Stopping::No, false) => Stopping::Drain { since: state.now },
                (Stopping::Drain { since }, false) => Stopping::Drain { since },
                (_, true) | (Stopping::Now, false) => Stopping::Now,
            };
            stop(state, stopping, actions);
            Reply::Accepted
        }
        Request::Kill(task) => kill(state, task, actions),
        Request::Logs { task, follow } => {
            let alive = state
                .runs
                .iter()
                .any(|run| run.task == task && run.agent_alive);
            let follow = follow && alive && state.log_clients.len() < CLIENTS_MAX;
            if follow {
                state.log_clients.retain(|(id, _)| *id != client);
                state.log_clients.push((client, task));
            }
            Reply::Log { task, follow }
        }
    };
    actions.push(Action::Reply { client, reply });
    assert!(state.down_clients.len() <= CLIENTS_MAX);
}

fn kill(state: &mut State, task: TaskId, actions: &mut Vec<Action>) -> Reply {
    let now = state.now;
    let running =
        (state.runs.iter_mut()).find(|run| run.task == task && run.phase == Phase::Running);
    if let Some(entry) = running {
        set_phase(entry, Phase::Stopping(StopReason::Killed), now);
        send_stop(entry, actions);
        return Reply::Done;
    }
    let known = state.tasks.iter().any(|known| known.id == task)
        || state.runs.iter().any(|run| run.task == task);
    let (code, message) = if known {
        (
            ErrorCode::NotRunning,
            format!("no agent is running on task #{}", task.0),
        )
    } else {
        (
            ErrorCode::UnknownTask,
            format!("the daemon does not know task #{}", task.0),
        )
    };
    assert!(
        state
            .runs
            .iter()
            .all(|run| run.task != task || run.phase != Phase::Running)
    );
    Reply::Refused { code, message }
}

// Ends the stream of each client following a task whose agent is gone.
fn tell_followers(state: &mut State, actions: &mut Vec<Action>) {
    let runs = &state.runs;
    let alive = |task: TaskId| runs.iter().any(|run| run.task == task && run.agent_alive);
    state.log_clients.retain(|(client, task)| {
        if alive(*task) {
            return true;
        }
        actions.push(Action::Reply {
            client: *client,
            reply: Reply::End,
        });
        false
    });
    assert!(state.log_clients.len() <= CLIENTS_MAX);
}

fn status_view(state: &State) -> StatusView {
    let now = state.now;
    let roles = &state.config.roles;
    let tasks = state.tasks.iter().map(|task| {
        let run = state.runs.iter().find(|run| run.task == task.id);
        TaskRow {
            id: task.id,
            project: task.project.clone(),
            state: task.state,
            role: task.role.map(|role| roles[role].label.clone()),
            profile: run.map(|run| state.profile(run.role, run.slot)),
            age_millis: run.map(|run| now.0.saturating_sub(run.attempt_start.0)),
            branch: task.branch.clone(),
        }
    });
    let view = StatusView {
        board: state.board,
        stopping: state.stopping,
        running: state.runs.len(),
        limit: state.config.agents_max,
        untracked: state.untracked(),
        tasks: tasks.collect(),
    };
    assert!(view.running <= usize::from(view.limit));
    assert!(view.tasks.len() <= state.config.tasks_max);
    view
}

fn agent_rows(state: &State) -> Vec<AgentRow> {
    let rows: Vec<AgentRow> = (state.runs.iter())
        .map(|run| AgentRow {
            task: run.task,
            role: state.config.roles[run.role].label.clone(),
            profile: state.profile(run.role, run.slot),
            phase: run.phase,
            age_millis: state.now.0.saturating_sub(run.attempt_start.0),
        })
        .collect();
    assert!(rows.len() <= usize::from(state.config.agents_max));
    assert_eq!(rows.len(), state.runs.len());
    rows
}

/// Tells each client waiting on `down` of every run this step removed.
fn tell_ended(state: &State, before: &[(RunId, TaskId, String)], actions: &mut Vec<Action>) {
    if state.down_clients.is_empty() {
        return;
    }
    let ended = before
        .iter()
        .filter(|(run, _, _)| state.run_index(*run).is_none());
    for (_, task, profile) in ended {
        for client in &state.down_clients {
            let line = format!("#{} {profile} ended", task.0);
            let reply = Reply::Line(line);
            actions.push(Action::Reply {
                client: *client,
                reply,
            });
        }
    }
    assert!(before.len() <= usize::from(AGENTS_MAX));
}

fn signal(state: &mut State, actions: &mut Vec<Action>) {
    let stopping = match state.stopping {
        Stopping::No => Stopping::Drain { since: state.now },
        Stopping::Drain { .. } | Stopping::Now => Stopping::Now,
    };
    stop(state, stopping, actions);
}

fn stop(state: &mut State, stopping: Stopping, actions: &mut Vec<Action>) {
    assert!(stopping != Stopping::No);
    let was_now = state.stopping == Stopping::Now;
    state.stopping = stopping;
    if state.stopping == Stopping::Now && !was_now {
        stop_now(state, actions);
    }
    assert!(state.stopping != Stopping::No);
    let running = state.runs.iter().any(|run| run.phase == Phase::Running);
    assert!(state.stopping != Stopping::Now || !running);
}

/// The task is not claimed again until it is learned anew or the next tick.
fn claim_refused(state: &mut State, run: RunId) {
    state.pending_claims.retain(|pending| pending.run != run);
    let Some(index) = run_in(state, run, Phase::Claiming) else {
        return;
    };
    let task = state.runs[index].task;
    remove_run(state, index);
    if let Some(entry) = state.tasks.iter_mut().find(|entry| entry.id == task) {
        entry.claim_refused = true;
    }
    assert!(state.runs.iter().all(|entry| entry.task != task));
    let next = next_claimable(state);
    assert!(next.is_none_or(|(index, _, _)| state.tasks[index].id != task));
}

fn track_feed(state: &mut State, feed: &BoardEvent) {
    assert!(state.tasks.len() <= state.config.tasks_max);
    state.feed_delivered = true;
    // The feed delivers in cursor order and a snapshot covers every open task as of its
    // cursor, so an event at or before the core's cursor is already reflected.
    if feed.cursor <= state.cursor {
        return;
    }
    state.cursor = feed.cursor;
    let Some(task_state) = feed.state else {
        return;
    };
    // A label event's labels are the whole task's only when the event is on the task itself.
    let labels_are_task = feed.kind != EventKind::MessagePosted;
    let index = state.tasks.iter().position(|task| task.id == feed.task);
    // A listing can show a task at a version the feed has not reached yet.
    let closed_at = state.closed.iter().find(|(id, _)| *id == feed.task);
    let version = index.map(|index| state.tasks[index].seen);
    if version.or(closed_at.map(|(_, at)| *at)) >= Some(feed.cursor) {
        return;
    }
    if is_closed(task_state) {
        match index {
            Some(index) => _ = state.tasks.remove(index),
            None => untracked_left(state),
        }
        state.untracked.retain(|(id, _)| *id != feed.task);
        note_closed(state, feed.task, feed.cursor);
        return;
    }
    let task = Task {
        id: feed.task,
        state: task_state,
        role: role_of(&state.config, &feed.labels),
        branch: label(&feed.labels, "branch"),
        project: project_of(&feed.channel),
        seen: feed.cursor,
        claim_refused: false,
    };
    match index {
        Some(index) if labels_are_task => state.tasks[index] = task,
        Some(index) => {
            let entry = &mut state.tasks[index];
            entry.state = task_state;
            entry.seen = feed.cursor;
            entry.claim_refused = false;
        }
        None if state.tasks.len() < state.config.tasks_max => {
            state.untracked.retain(|(id, _)| *id != feed.task);
            state.tasks.push(task);
            untracked_left(state);
        }
        None => note_untracked(state, feed.task, feed.cursor),
    }
    assert!(
        state
            .tasks
            .iter()
            .filter(|task| task.id == feed.task)
            .count()
            <= 1
    );
    reserve_branches(state);
    assert!(state.untracked.len() <= UNTRACKED_MAX);
}

/// The project a task's channel names under `work/`, if it names one.
fn project_of(channel: &str) -> Option<String> {
    let project = channel
        .strip_prefix("work/")
        .filter(|project| !project.is_empty());
    assert!(project.is_none_or(|project| channel.ends_with(project)));
    project.map(str::to_string)
}

/// A task the last listing may have left out became tracked or closed, so the bound drops.
fn untracked_left(state: &mut State) {
    let before = state.left_out;
    if let Some(floor) = state.left_out.as_mut() {
        *floor = floor.saturating_sub(1);
    }
    assert_eq!(before.is_some(), state.left_out.is_some());
    assert!(state.left_out <= before);
}

/// A run whose branch was unknown reserves it once its task's branch is learned and free.
fn reserve_branches(state: &mut State) {
    for index in 0..state.runs.len() {
        if state.runs[index].branch.is_some() {
            continue;
        }
        let task = state
            .tasks
            .iter()
            .find(|task| task.id == state.runs[index].task);
        let Some(branch) = task.and_then(|task| task.branch.clone()) else {
            continue;
        };
        if !state
            .runs
            .iter()
            .any(|run| run.branch.as_ref() == Some(&branch))
        {
            state.runs[index].branch = Some(branch);
        }
    }
    assert!(state.runs.len() <= usize::from(state.config.agents_max));
    assert!(branches_unique(&state.runs));
}

fn branches_unique(runs: &[Run]) -> bool {
    runs.iter().enumerate().all(|(index, run)| {
        run.branch.is_none()
            || runs[index + 1..]
                .iter()
                .all(|other| other.branch != run.branch)
    })
}

/// Past `UNTRACKED_MAX` the count is a lower bound until a snapshot newer than the overflow.
fn note_untracked(state: &mut State, task: TaskId, at: Cursor) {
    if let Some(entry) = state.untracked.iter_mut().find(|(id, _)| *id == task) {
        entry.1 = entry.1.max(at);
    } else if state.untracked.len() < UNTRACKED_MAX {
        state.untracked.push((task, at));
    } else {
        state.untracked_overflow = Some(state.untracked_overflow.map_or(at, |old| old.max(at)));
    }
    assert!(state.untracked.len() <= UNTRACKED_MAX);
    assert!(state.tasks.iter().all(|entry| entry.id != task));
}

/// Past `CLOSED_MAX` a snapshot older than the unrecorded closing is not trusted.
fn note_closed(state: &mut State, task: TaskId, at: Cursor) {
    assert!(state.tasks.iter().all(|entry| entry.id != task));
    state.closed.retain(|(id, _)| *id != task);
    if state.closed.len() < CLOSED_MAX {
        state.closed.push((task, at));
    } else {
        state.closed_unrecorded = state.closed_unrecorded.max(at);
    }
    assert!(state.closed.len() <= CLOSED_MAX);
}

/// Replaces what the core knows with a snapshot, except what is newer by the task's version.
fn reconcile(state: &mut State, tasks: &[ListedTask], cursor: Cursor, left_out: usize) {
    if cursor < state.closed_unrecorded {
        return;
    }
    state.closed.retain(|(_, at)| *at > cursor);
    state.untracked.retain(|(_, at)| *at > cursor);
    if state.untracked_overflow.is_some_and(|at| at <= cursor) {
        state.untracked_overflow = None;
    }
    let listed = |id: TaskId| {
        (tasks.iter())
            .find(|task| task.view.id == id)
            .map(|task| &task.view)
    };
    // A task the listing omits is gone unless the feed told of it after the listing began.
    state.tasks.retain(|task| match listed(task.id) {
        Some(view) => view.version <= task.seen,
        None => task.seen > cursor,
    });
    // A tracked task the listing omits may be one of those it left out.
    let kept = (state.tasks.iter())
        .filter(|task| listed(task.id).is_none())
        .count();
    let mut listed_untracked: usize = 0;
    // Tasks with a run go first, so a run's task stays known whatever the overflow.
    for with_run in [true, false] {
        for ListedTask { view, channel } in tasks {
            let has_run = state.runs.iter().any(|run| run.task == view.id);
            let newer = state.tasks.iter().any(|task| task.id == view.id)
                || (state.closed.iter()).any(|(id, at)| *id == view.id && *at >= view.version);
            if has_run != with_run || newer {
                continue;
            }
            if is_closed(view.state) {
                (state.untracked).retain(|(id, at)| *id != view.id || *at > view.version);
                if view.version > cursor {
                    note_closed(state, view.id, view.version);
                }
                continue;
            }
            if state.tasks.len() == state.config.tasks_max {
                note_untracked(state, view.id, view.version);
                listed_untracked = listed_untracked.saturating_add(1);
                continue;
            }
            state.untracked.retain(|(id, _)| *id != view.id);
            state.tasks.push(Task {
                id: view.id,
                state: view.state,
                role: role_of(&state.config, &view.labels),
                branch: label(&view.labels, "branch"),
                project: project_of(channel),
                seen: view.version,
                claim_refused: false,
            });
        }
    }
    // The tasks left out are none of those listed, so both counts add to a lower bound.
    let left_out_untracked = left_out.saturating_sub(kept);
    state.left_out = (left_out > 0).then(|| listed_untracked.saturating_add(left_out_untracked));
    reserve_branches(state);
    assert!(state.tasks.len() <= state.config.tasks_max);
    let version = |id: TaskId| listed(id).map(|view| view.version);
    assert!(
        state
            .tasks
            .iter()
            .all(|task| version(task.id) <= Some(task.seen))
    );
    let untracked = |id: TaskId| state.untracked.iter().any(|(other, _)| *other == id);
    assert!(state.tasks.iter().all(|task| !untracked(task.id)));
}

fn is_closed(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled
    )
}

fn label(labels: &[(String, String)], key: &str) -> Option<String> {
    assert!(!key.is_empty());
    assert!(labels.len() <= LABELS_MAX);
    let found = labels.iter().find(|(name, _)| name == key);
    found.map(|(_, value)| value.clone())
}

fn role_of(config: &Config, labels: &[(String, String)]) -> Option<usize> {
    assert!(labels.len() <= LABELS_MAX);
    let role = labels.iter().find(|(name, _)| name == "role")?;
    let index = config.roles.iter().position(|spec| spec.label == role.1);
    assert!(index.is_none_or(|index| index < config.roles.len()));
    index
}

fn mark_down(state: &mut State) {
    let before = state.board;
    if state.board == BoardHealth::Up {
        state.board = BoardHealth::Down { since: state.now };
    }
    assert!(state.board != BoardHealth::Up);
    assert!(before == BoardHealth::Up || state.board == before);
}

fn board_failed(state: &mut State, call: Call) {
    mark_down(state);
    let now = state.now;
    let pause = state.config.settle_pause_millis;
    for pending in state.pending_claims.iter_mut() {
        let closing = call == Call::Release(pending.run) || call == Call::Settle(pending.run);
        if closing && pending.releasing {
            pending.releasing = false;
            pending.retry_at = Timestamp(now.0.saturating_add(pause));
        }
    }
    // A claim that failed outright was not granted by this call.
    state
        .pending_claims
        .retain(|pending| call != Call::Claim(pending.run));
    assert!(state.board != BoardHealth::Up);
    let run = match call {
        Call::ListTasks => {
            state.listing = false;
            return;
        }
        Call::Claim(run) | Call::Renew(run) | Call::Settle(run) | Call::Release(run) => run,
    };
    let Some(index) = state.run_index(run) else {
        return;
    };
    let tries_max = state.config.settle_tries_max;
    let entry = &mut state.runs[index];
    match call {
        // Whether the board granted it is unknown; an unrenewed claim lapses within the lease.
        Call::Claim(_) if entry.phase == Phase::Claiming => remove_run(state, index),
        Call::Renew(_) => entry.renewing = false,
        Call::Settle(_) | Call::Release(_) if entry.settling => {
            entry.settling = false;
            entry.settle_tries = entry.settle_tries.saturating_add(1);
            entry.retry_at = Timestamp(now.0.saturating_add(pause));
            if entry.settle_tries >= tries_max {
                give_up_claim(state, index);
            }
        }
        _ => {}
    }
    let index = state.run_index(run);
    assert!(index.is_none_or(|index| state.runs[index].settle_tries < tries_max));
}

fn give_up_claim(state: &mut State, index: usize) {
    let lease = state.config.lease_millis;
    let entry = &state.runs[index];
    assert!(holds_claim(entry.phase));
    let mut until = entry.lease_until;
    if entry.renewing {
        // A renewal in flight may extend the lease by up to one lease from when it was sent.
        until = until.max(Timestamp(state.now.0.saturating_add(lease)));
    }
    let epoch = entry.epoch;
    give_up(state, index, until, Some(epoch));
}

// The board may hold the run's claim until `until`; the core does not exit before it lapses.
fn give_up(state: &mut State, index: usize, until: Timestamp, epoch: Option<u64>) {
    // Claims are sent only while pending and claiming runs number below `agents_max`, so
    // runs and pending claims together stay below `PENDING_CLAIMS_MAX`.
    assert!(state.pending_claims.len() + state.runs.len() <= PENDING_CLAIMS_MAX);
    let now = state.now;
    let entry = &state.runs[index];
    // A closing write in flight is answered like the pending claim's own release.
    let (run, task, releasing) = (entry.id, entry.task, entry.settling);
    let profile = state.profile(entry.role, entry.slot);
    remove_run(state, index);
    if now < until {
        state.pending_claims.push(PendingClaim {
            run,
            task,
            profile,
            until,
            epoch,
            releasing,
            retry_at: now,
        });
    }
    assert!(state.run_index(run).is_none());
}

fn remove_run(state: &mut State, index: usize) {
    assert!(index < state.runs.len());
    let removed = state.runs.remove(index);
    assert!(state.run_index(removed.id).is_none());
    assert!(!removed.agent_alive);
}

fn set_phase(run: &mut Run, phase: Phase, now: Timestamp) {
    assert!(run.phase != Phase::Abandoning || phase == Phase::Abandoning);
    assert!(run.phase != phase || phase == Phase::Abandoning);
    run.phase = phase;
    run.phase_start = now;
}

fn run_in(state: &State, run: RunId, phase: Phase) -> Option<usize> {
    let index = state.run_index(run)?;
    assert!(index < usize::from(state.config.agents_max));
    assert_eq!(state.runs[index].id, run);
    (state.runs[index].phase == phase).then_some(index)
}

fn on_claimed(
    state: &mut State,
    run: RunId,
    epoch: u64,
    until: Timestamp,
    actions: &mut Vec<Action>,
) {
    state.board = BoardHealth::Up;
    let now = state.now;
    let given_up = state
        .pending_claims
        .iter()
        .any(|pending| pending.run == run);
    assert!(!given_up || state.run_index(run).is_none());
    if let Some(pending) = state.pending_claims.iter_mut().find(|p| p.run == run) {
        pending.epoch = Some(epoch);
        pending.retry_at = now;
        return;
    }
    let Some(index) = run_in(state, run, Phase::Claiming) else {
        return;
    };
    let entry = &mut state.runs[index];
    entry.epoch = epoch;
    entry.lease_until = until;
    if state.stopping == Stopping::Now {
        begin_settle(state, index, Settlement::Release, actions);
        return;
    }
    set_phase(entry, Phase::Preparing, now);
    let (run, task) = (entry.id, entry.task);
    assert_eq!(
        state.runs.iter().filter(|entry| entry.task == task).count(),
        1
    );
    actions.push(Action::Prepare { run, task });
}

fn on_prepared(
    state: &mut State,
    run: RunId,
    branch: Option<String>,
    role: &str,
    actions: &mut Vec<Action>,
) {
    let Some(index) = run_in(state, run, Phase::Preparing) else {
        return;
    };
    // The branch preparation found is the one the agent would work on, whatever was reserved.
    if branch.is_some() {
        state.runs[index].branch = branch;
    }
    let mine = state.runs[index].branch.as_deref();
    let taken = mine.is_some()
        && (state.runs.iter()).any(|other| other.id != run && other.branch.as_deref() == mine);
    if taken {
        // The branch belongs to the run that reserved it first; this one gives the task back.
        state.runs[index].branch = None;
        begin_settle(state, index, Settlement::Release, actions);
        return;
    }
    // A task relabeled after its claim goes back, to be claimed under its new role, slot and
    // profile, so the builders limit counts each agent by the role it launches as.
    let relabeled = state.config.roles[state.runs[index].role].label != role;
    if relabeled || state.stopping == Stopping::Now {
        begin_settle(state, index, Settlement::Release, actions);
        return;
    }
    let profile = state.profile(state.runs[index].role, state.runs[index].slot);
    assert!(branches_unique(&state.runs));
    let now = state.now;
    let entry = &mut state.runs[index];
    assert!(!entry.agent_alive);
    set_phase(entry, Phase::Running, now);
    entry.attempt_start = now;
    entry.agent_alive = true;
    let (run, task) = (entry.id, entry.task);
    actions.push(Action::Spawn { run, task, profile });
}

fn on_agent_exited(state: &mut State, run: RunId, actions: &mut Vec<Action>) {
    let Some(index) = state.run_index(run) else {
        return;
    };
    let now = state.now;
    let entry = &mut state.runs[index];
    entry.agent_alive = false;
    entry.agent_lost = false;
    match entry.phase {
        Phase::Running => {
            set_phase(entry, Phase::Judging, now);
            let (run, task) = (entry.id, entry.task);
            actions.push(Action::Judge { run, task });
        }
        Phase::Stopping(StopReason::TimeLimit) => {
            let reason = "the attempt reached its time limit".to_string();
            let settlement = Settlement::Verdict(Verdict::Fail { reason });
            begin_settle(state, index, settlement, actions);
        }
        Phase::Stopping(StopReason::StopNow) => {
            begin_settle(state, index, Settlement::Release, actions);
        }
        Phase::Stopping(StopReason::Killed) => {
            let reason = KILLED_REASON.to_string();
            let settlement = Settlement::Verdict(Verdict::Fail { reason });
            begin_settle(state, index, settlement, actions);
        }
        Phase::Abandoning => remove_run(state, index),
        Phase::Claiming | Phase::Preparing | Phase::Judging | Phase::Settling => {}
    }
    let index = state.run_index(run);
    assert!(index.is_none_or(|index| state.runs[index].phase != Phase::Running));
    assert!(index.is_none_or(|index| !state.runs[index].agent_alive));
}

fn begin_settle(
    state: &mut State,
    index: usize,
    settlement: Settlement,
    actions: &mut Vec<Action>,
) {
    let now = state.now;
    let entry = &mut state.runs[index];
    assert!(!entry.agent_alive);
    assert!(entry.settlement.is_none());
    set_phase(entry, Phase::Settling, now);
    entry.settlement = Some(settlement);
    entry.retry_at = now;
    try_settle(state, index, actions);
}

fn try_settle(state: &mut State, index: usize, actions: &mut Vec<Action>) {
    let up = state.board == BoardHealth::Up;
    let now = state.now;
    let tries_max = state.config.settle_tries_max;
    let entry = &mut state.runs[index];
    assert_eq!(entry.phase, Phase::Settling);
    assert!(entry.settle_tries < tries_max);
    if !up || entry.settling || now < entry.retry_at {
        return;
    }
    let (run, task, epoch) = (entry.id, entry.task, entry.epoch);
    let (role, slot) = (entry.role, entry.slot);
    // After stop-now no attempt counts: whatever was decided, the claim is released.
    if state.stopping == Stopping::Now {
        entry.settlement = Some(Settlement::Release);
    }
    let Some(settlement) = entry.settlement.clone() else {
        return;
    };
    entry.settling = true;
    actions.push(match settlement {
        Settlement::Release => Action::Release {
            run,
            task,
            epoch,
            profile: state.profile(role, slot),
        },
        settlement => Action::Settle {
            run,
            task,
            epoch,
            settlement,
        },
    });
}

fn abandon(state: &mut State, index: usize, actions: &mut Vec<Action>) {
    let now = state.now;
    let entry = &mut state.runs[index];
    assert!(index < AGENTS_MAX as usize);
    if !entry.agent_alive {
        remove_run(state, index);
        return;
    }
    let already_stopping = matches!(entry.phase, Phase::Stopping(_) | Phase::Abandoning);
    let started = entry.phase_start;
    set_phase(entry, Phase::Abandoning, now);
    if already_stopping {
        // The stop already sent keeps its deadline.
        entry.phase_start = started;
    } else {
        send_stop(entry, actions);
    }
    assert!(entry.stop_tries >= 1);
}

fn stop_now(state: &mut State, actions: &mut Vec<Action>) {
    assert_eq!(state.stopping, Stopping::Now);
    let now = state.now;
    for entry in state.runs.iter_mut() {
        if entry.phase == Phase::Running {
            set_phase(entry, Phase::Stopping(StopReason::StopNow), now);
            send_stop(entry, actions);
        }
    }
    assert!(state.runs.iter().all(|run| run.phase != Phase::Running));
}

fn send_stop(entry: &mut Run, actions: &mut Vec<Action>) {
    assert!(entry.agent_alive);
    assert!(matches!(
        entry.phase,
        Phase::Stopping(_) | Phase::Abandoning
    ));
    entry.stop_tries = 1;
    let (run, task) = (entry.id, entry.task);
    actions.push(Action::StopAgent { run, task });
}

fn holds_claim(phase: Phase) -> bool {
    !matches!(phase, Phase::Claiming | Phase::Abandoning)
}

fn expire_leases(state: &mut State, actions: &mut Vec<Action>) {
    let now = state.now;
    let mut index = 0;
    while index < state.runs.len() {
        let entry = &state.runs[index];
        let lapsed = holds_claim(entry.phase) && !entry.renewing && entry.lease_until <= now;
        let before = state.runs.len();
        if lapsed {
            abandon(state, index, actions);
        }
        if state.runs.len() == before {
            index += 1;
        }
    }
    assert!(state.runs.len() <= usize::from(state.config.agents_max));
    let unrenewed = |run: &Run| holds_claim(run.phase) && !run.renewing;
    assert!(
        state
            .runs
            .iter()
            .all(|run| !unrenewed(run) || run.lease_until > now)
    );
}

fn on_time(state: &mut State, actions: &mut Vec<Action>) {
    let now = state.now;
    let config = &state.config;
    let reconcile_at = state
        .last_reconcile
        .0
        .saturating_add(config.reconcile_millis);
    if now.0 >= reconcile_at && !state.listing && state.board == BoardHealth::Up {
        state.last_reconcile = now;
        state.listing = true;
        actions.push(Action::ListTasks);
    }
    if let Stopping::Drain { since } = state.stopping
        && now.0 >= since.0.saturating_add(config.drain_millis_max)
    {
        state.stopping = Stopping::Now;
        stop_now(state, actions);
    }
    let renew_lead = state.config.lease_millis / 3 * 2;
    for index in 0..state.runs.len() {
        let entry = &mut state.runs[index];
        if holds_claim(entry.phase)
            && !entry.renewing
            && now.0 >= entry.lease_until.0.saturating_sub(renew_lead)
        {
            entry.renewing = true;
            let (run, task) = (entry.id, entry.task);
            actions.push(Action::Renew { run, task });
        }
        if entry.phase == Phase::Settling {
            try_settle(state, index, actions);
        }
    }
    let due = |run: &Run| now.0 >= run.lease_until.0.saturating_sub(renew_lead);
    assert!(
        state
            .runs
            .iter()
            .all(|run| !holds_claim(run.phase) || run.renewing || !due(run))
    );
    release_pending(state, actions);
    assert!(actions.len() <= ACTIONS_MAX);
}

/// Releases late grants of given-up claims, and forgets those whose grant has lapsed.
fn release_pending(state: &mut State, actions: &mut Vec<Action>) {
    let now = state.now;
    let up = state.board == BoardHealth::Up;
    state.pending_claims.retain(|pending| now < pending.until);
    for pending in state.pending_claims.iter_mut() {
        let Some(epoch) = pending.epoch else {
            continue;
        };
        if up && !pending.releasing && now >= pending.retry_at {
            pending.releasing = true;
            let (run, task, profile) = (pending.run, pending.task, pending.profile.clone());
            actions.push(Action::Release {
                run,
                task,
                epoch,
                profile,
            });
        }
    }
    assert!(state.pending_claims.len() <= PENDING_CLAIMS_MAX);
    let held = |pending: &PendingClaim| now < pending.until;
    assert!(state.pending_claims.iter().all(held));
}

fn claim_ready(state: &mut State, actions: &mut Vec<Action>) {
    if state.board != BoardHealth::Up || state.stopping != Stopping::No {
        return;
    }
    let agents_max = usize::from(state.config.agents_max);
    loop {
        let claiming = state.runs.iter().filter(|run| run.phase == Phase::Claiming);
        // Every claim in flight must fit among the pending claims if it is given up.
        let unanswered = state.pending_claims.len() + claiming.count();
        if state.runs.len() >= agents_max || unanswered >= agents_max {
            break;
        }
        let Some((task, role, slot)) = next_claimable(state) else {
            break;
        };
        let id = RunId(state.next_run);
        state.next_run = state.next_run.saturating_add(1);
        let profile = state.profile(role, slot);
        let now = state.now;
        let branch = state.tasks[task].branch.clone();
        let task = state.tasks[task].id;
        state.runs.push(Run {
            id,
            task,
            role,
            slot,
            phase: Phase::Claiming,
            epoch: 0,
            lease_until: now,
            attempt_start: now,
            phase_start: now,
            renewing: false,
            settlement: None,
            settle_tries: 0,
            settling: false,
            retry_at: now,
            agent_alive: false,
            branch,
            stop_tries: 0,
            agent_lost: false,
        });
        actions.push(Action::Claim {
            run: id,
            task,
            profile,
        });
        assert!(state.next_run > id.0);
    }
    assert!(state.runs.len() <= agents_max);
}

/// The oldest claimable task, its role, and the lowest free slot of that role; a task of a
/// building role is claimable only while fewer than `builders_max` building runs exist.
fn next_claimable(state: &State) -> Option<(usize, usize, u8)> {
    let roles = &state.config.roles;
    let builders = state.runs.iter().filter(|run| roles[run.role].builds);
    let builders_full = builders.count() >= usize::from(state.config.builders_max);
    let mut best: Option<(usize, usize, u8)> = None;
    for (index, task) in state.tasks.iter().enumerate() {
        let role = task
            .role
            .filter(|role| !task.claim_refused && !(builders_full && roles[*role].builds));
        let Some(role) = role else {
            continue;
        };
        let taken = state
            .runs
            .iter()
            .any(|run| run.task == task.id || (task.branch.is_some() && run.branch == task.branch));
        if task.state != TaskState::Ready || taken {
            continue;
        }
        let slots = state.config.roles[role].slots;
        let free = (1..=slots).find(|slot| {
            !state
                .runs
                .iter()
                .any(|run| run.role == role && run.slot == *slot)
        });
        let older = best.is_none_or(|(best, _, _)| task.id.0 < state.tasks[best].id.0);
        if let Some(slot) = free
            && older
        {
            best = Some((index, role, slot));
        }
    }
    assert!(best.is_none_or(|(index, _, _)| index < state.tasks.len()));
    let claimable = |index: usize| {
        let task = &state.tasks[index];
        task.state == TaskState::Ready && !task.claim_refused && task.role.is_some()
    };
    assert!(best.is_none_or(|(index, _, slot)| claimable(index) && slot >= 1));
    assert!(best.is_none_or(|(_, role, _)| !(builders_full && roles[role].builds)));
    best
}

fn expire_phases(state: &mut State, actions: &mut Vec<Action>) {
    let now = state.now;
    let mut index = 0;
    while index < state.runs.len() {
        let entry = &state.runs[index];
        let limit = state.config.phase_millis_max(entry.phase);
        let due = !entry.agent_lost && now.0 >= entry.phase_start.0.saturating_add(limit);
        let before = state.runs.len();
        if due {
            phase_expired(state, index, actions);
        }
        if state.runs.len() == before {
            index += 1;
        }
    }
    assert!(state.runs.len() <= usize::from(state.config.agents_max));
    let config = &state.config;
    let within = |run: &Run| {
        now.0
            < run
                .phase_start
                .0
                .saturating_add(config.phase_millis_max(run.phase))
    };
    assert!(state.runs.iter().all(|run| run.agent_lost || within(run)));
}

fn phase_expired(state: &mut State, index: usize, actions: &mut Vec<Action>) {
    assert!(index < state.runs.len());
    let now = state.now;
    let entry = &mut state.runs[index];
    assert!(!entry.agent_lost);
    let (run, task) = (entry.id, entry.task);
    match entry.phase {
        // Whether the board granted it is unknown; a late grant is released.
        Phase::Claiming => {
            let lease = state.config.lease_millis;
            let until = Timestamp(entry.phase_start.0.saturating_add(lease));
            give_up(state, index, until, None);
        }
        Phase::Preparing | Phase::Judging => {
            begin_settle(state, index, Settlement::Release, actions);
        }
        Phase::Running => {
            set_phase(entry, Phase::Stopping(StopReason::TimeLimit), now);
            send_stop(entry, actions);
        }
        // The closing write is abandoned, as when its tries run out.
        Phase::Settling => give_up_claim(state, index),
        // Only an observed exit ends the agent; past the tries it is lost, not gone.
        Phase::Stopping(_) | Phase::Abandoning if entry.stop_tries < STOP_TRIES_MAX => {
            entry.stop_tries = entry.stop_tries.saturating_add(1);
            entry.phase_start = now;
            actions.push(Action::StopAgent { run, task });
        }
        Phase::Stopping(_) | Phase::Abandoning => entry.agent_lost = true,
    }
}

fn finish_stopping(state: &mut State, actions: &mut Vec<Action>) {
    let busy = !state.runs.is_empty() || !state.pending_claims.is_empty();
    if state.stopping == Stopping::No || busy || state.exited {
        return;
    }
    state.exited = true;
    for client in state.down_clients.drain(..) {
        actions.push(Action::Reply {
            client,
            reply: Reply::End,
        });
    }
    actions.push(Action::Exit(0));
    assert!(state.runs.is_empty());
    assert!(state.pending_claims.is_empty());
}
