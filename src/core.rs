//! The daemon's decisions for workers: one pure `step` from a state and an event to actions.

use crate::board::{BOARD_CALL_SECONDS_MAX, TaskId, TaskState, TaskView, Timestamp};
use crate::feed::{BoardEvent, Cursor, EventKind};
use crate::roles::{AGENTS_MAX, RoleSpec};
use crate::settle::Verdict;

/// Upper bound on the open tasks the core tracks.
pub const TASKS_MAX: usize = 256;
/// Upper bound on the roles in the table the core is given.
pub const ROLES_MAX: usize = 16;
/// Upper bound on the actions one step emits: a few per run, plus a few of its own.
pub const ACTIONS_MAX: usize = 6 * AGENTS_MAX as usize + 8;
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
/// Upper bound on claims whose answer came after their run was given up.
pub const PENDING_CLAIMS_MAX: usize = AGENTS_MAX as usize;
/// Upper bound on the distinct untracked tasks the core counts.
pub const UNTRACKED_MAX: usize = 4 * TASKS_MAX;
/// How many feed restarts in a row are allowed before the board counts as down.
pub const FEED_RESTARTS_MAX: u8 = 10;

/// One role as the core sees it: its label, its profile's model part and its slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub label: String,
    pub model_slug: String,
    pub slots: u8,
}

/// The role table and limits the core runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub roles: Vec<Role>,
    pub agents_max: u8,
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
            })
            .collect();
        Config {
            roles,
            agents_max: AGENTS_MAX,
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
}

/// Why a running agent is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    TimeLimit,
    StopNow,
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

/// A claim given up while unanswered: if the board granted it, the grant is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClaim {
    pub run: RunId,
    pub task: TaskId,
    /// When the claim was sent; past one lease any grant has lapsed.
    pub since: Timestamp,
    /// The granted epoch, once a late `Claimed` arrived.
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
    Snapshot {
        tasks: Vec<TaskView>,
        cursor: Cursor,
    },
    Claimed {
        run: RunId,
        epoch: u64,
        until: Timestamp,
    },
    ClaimRefused(RunId),
    /// The worktree is ready; `branch` is the branch preparation found, if any.
    Prepared {
        run: RunId,
        branch: Option<String>,
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
    },
    ListTasks,
    RestartFeed(Cursor),
    Exit(i32),
}

/// Everything the core knows; changed only by `step`.
#[derive(Debug, Clone)]
pub struct State {
    config: Config,
    tasks: Vec<Task>,
    untracked: Vec<TaskId>,
    untracked_overflow: bool,
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
            untracked_overflow: false,
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
            exited: false,
        };
        assert!(state.runs.is_empty());
        state
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }
    /// Tasks seen but not tracked because `tasks_max` was reached.
    pub fn untracked(&self) -> Untracked {
        let count = self.untracked.len();
        assert!(count <= UNTRACKED_MAX);
        if self.untracked_overflow {
            Untracked::AtLeast(count)
        } else {
            Untracked::Exactly(count)
        }
    }
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }
    pub fn pending_claims(&self) -> &[PendingClaim] {
        &self.pending_claims
    }
    pub fn board(&self) -> BoardHealth {
        self.board
    }
    pub fn stopping(&self) -> Stopping {
        self.stopping
    }
    pub fn now(&self) -> Timestamp {
        self.now
    }
    pub fn exited(&self) -> bool {
        self.exited
    }

    fn run_index(&self, id: RunId) -> Option<usize> {
        self.runs.iter().position(|run| run.id == id)
    }

    fn profile(&self, role: usize, slot: u8) -> String {
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
    apply(state, event, &mut actions);
    expire_leases(state, &mut actions);
    on_time(state, &mut actions);
    expire_phases(state, &mut actions);
    claim_ready(state, &mut actions);
    finish_stopping(state, &mut actions);
    assert!(actions.len() <= ACTIONS_MAX);
    assert!(state.runs.len() <= usize::from(state.config.agents_max));
    assert!(state.tasks.len() <= state.config.tasks_max);
    actions
}

fn apply(state: &mut State, event: Event, actions: &mut Vec<Action>) {
    match event {
        Event::Tick(now) => {
            state.now = now;
            if state.board != BoardHealth::Up && !state.listing {
                state.listing = true;
                actions.push(Action::ListTasks);
            }
        }
        Event::Feed(feed) => track_feed(state, &feed),
        Event::FeedLost => feed_lost(state, actions),
        Event::FeedResumed(cursor) => {
            state.feed_restarts = 0;
            state.cursor = state.cursor.max(cursor);
        }
        Event::Snapshot { tasks, cursor } => {
            state.listing = false;
            state.board = BoardHealth::Up;
            state.cursor = state.cursor.max(cursor);
            reconcile(state, &tasks);
        }
        Event::BoardFailed(call) => board_failed(state, call),
        Event::Signal => signal(state, actions),
        Event::Claimed { run, epoch, until } => on_claimed(state, run, epoch, until, actions),
        Event::ClaimRefused(run) => {
            state.pending_claims.retain(|pending| pending.run != run);
            if let Some(index) = run_in(state, run, Phase::Claiming) {
                remove_run(state, index);
            }
        }
        Event::Prepared { run, branch } => on_prepared(state, run, branch, actions),
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

fn feed_lost(state: &mut State, actions: &mut Vec<Action>) {
    state.feed_restarts = state.feed_restarts.saturating_add(1);
    if state.feed_restarts > FEED_RESTARTS_MAX {
        mark_down(state);
    }
    actions.push(Action::RestartFeed(state.cursor));
    if !state.listing {
        state.listing = true;
        actions.push(Action::ListTasks);
    }
    assert!(state.listing);
}

fn signal(state: &mut State, actions: &mut Vec<Action>) {
    state.stopping = match state.stopping {
        Stopping::No => Stopping::Drain { since: state.now },
        Stopping::Drain { .. } | Stopping::Now => Stopping::Now,
    };
    if state.stopping == Stopping::Now {
        stop_now(state, actions);
    }
    assert!(state.stopping != Stopping::No);
}

fn track_feed(state: &mut State, feed: &BoardEvent) {
    assert!(state.tasks.len() <= state.config.tasks_max);
    state.cursor = state.cursor.max(feed.cursor);
    let Some(task_state) = feed.state else {
        return;
    };
    // A label event's labels are the whole task's only when the event is on the task itself.
    let labels_are_task = feed.kind != EventKind::MessagePosted;
    let index = state.tasks.iter().position(|task| task.id == feed.task);
    if is_closed(task_state) {
        if let Some(index) = index {
            state.tasks.remove(index);
        }
        state.untracked.retain(|id| *id != feed.task);
        return;
    }
    let task = Task {
        id: feed.task,
        state: task_state,
        role: role_of(&state.config, &feed.labels),
        branch: label(&feed.labels, "branch"),
    };
    match index {
        Some(index) if labels_are_task => state.tasks[index] = task,
        Some(index) => state.tasks[index].state = task_state,
        None if state.tasks.len() < state.config.tasks_max => {
            state.untracked.retain(|id| *id != feed.task);
            state.tasks.push(task);
        }
        None => note_untracked(state, feed.task),
    }
    reserve_branches(state);
    assert!(state.untracked.len() <= UNTRACKED_MAX);
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
}

/// Past `UNTRACKED_MAX` the count is a lower bound until the next snapshot.
fn note_untracked(state: &mut State, task: TaskId) {
    if !state.untracked.contains(&task) {
        if state.untracked.len() < UNTRACKED_MAX {
            state.untracked.push(task);
        } else {
            state.untracked_overflow = true;
        }
    }
    assert!(state.untracked.len() <= UNTRACKED_MAX);
}

fn reconcile(state: &mut State, views: &[TaskView]) {
    state.tasks.clear();
    state.untracked.clear();
    state.untracked_overflow = false;
    // Tasks with a run go first, so a run's task stays known whatever the overflow.
    for with_run in [true, false] {
        for view in views {
            let has_run = state.runs.iter().any(|run| run.task == view.id);
            if is_closed(view.state) || has_run != with_run {
                continue;
            }
            if state.tasks.len() == state.config.tasks_max {
                note_untracked(state, view.id);
                continue;
            }
            state.tasks.push(Task {
                id: view.id,
                state: view.state,
                role: role_of(&state.config, &view.labels),
                branch: label(&view.labels, "branch"),
            });
        }
    }
    reserve_branches(state);
    assert!(state.tasks.len() <= state.config.tasks_max);
    assert!(state.tasks.len() <= views.len());
}

fn is_closed(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled
    )
}

fn label(labels: &[(String, String)], key: &str) -> Option<String> {
    assert!(!key.is_empty());
    let found = labels.iter().find(|(name, _)| name == key);
    found.map(|(_, value)| value.clone())
}

fn role_of(config: &Config, labels: &[(String, String)]) -> Option<usize> {
    let role = labels.iter().find(|(name, _)| name == "role")?;
    let index = config.roles.iter().position(|spec| spec.label == role.1);
    assert!(index.is_none_or(|index| index < config.roles.len()));
    index
}

fn mark_down(state: &mut State) {
    if state.board == BoardHealth::Up {
        state.board = BoardHealth::Down { since: state.now };
    }
    assert!(state.board != BoardHealth::Up);
}

fn board_failed(state: &mut State, call: Call) {
    mark_down(state);
    let now = state.now;
    let pause = state.config.settle_pause_millis;
    for pending in state.pending_claims.iter_mut() {
        if call == Call::Release(pending.run) && pending.releasing {
            pending.releasing = false;
            pending.retry_at = Timestamp(now.0.saturating_add(pause));
        }
    }
    // A claim that failed outright was not granted by this call.
    state
        .pending_claims
        .retain(|pending| call != Call::Claim(pending.run));
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
                remove_run(state, index);
            }
        }
        _ => {}
    }
}

fn remove_run(state: &mut State, index: usize) {
    assert!(index < state.runs.len());
    let removed = state.runs.remove(index);
    assert!(state.run_index(removed.id).is_none());
}

fn set_phase(run: &mut Run, phase: Phase, now: Timestamp) {
    assert!(run.phase != Phase::Abandoning || phase == Phase::Abandoning);
    run.phase = phase;
    run.phase_start = now;
}

fn run_in(state: &State, run: RunId, phase: Phase) -> Option<usize> {
    let index = state.run_index(run)?;
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
    actions.push(Action::Prepare { run, task });
}

fn on_prepared(state: &mut State, run: RunId, branch: Option<String>, actions: &mut Vec<Action>) {
    let Some(index) = run_in(state, run, Phase::Preparing) else {
        return;
    };
    if state.runs[index].branch.is_none() {
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
    if state.stopping == Stopping::Now {
        begin_settle(state, index, Settlement::Release, actions);
        return;
    }
    let profile = state.profile(state.runs[index].role, state.runs[index].slot);
    let now = state.now;
    let entry = &mut state.runs[index];
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
        Phase::Abandoning => remove_run(state, index),
        Phase::Claiming | Phase::Preparing | Phase::Judging | Phase::Settling => {}
    }
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
    let entry = &mut state.runs[index];
    assert_eq!(entry.phase, Phase::Settling);
    if !up || entry.settling || now < entry.retry_at {
        return;
    }
    let (run, task, epoch) = (entry.id, entry.task, entry.epoch);
    // After stop-now no attempt counts: whatever was decided, the claim is released.
    if state.stopping == Stopping::Now {
        entry.settlement = Some(Settlement::Release);
    }
    let Some(settlement) = entry.settlement.clone() else {
        return;
    };
    entry.settling = true;
    actions.push(match settlement {
        Settlement::Release => Action::Release { run, task, epoch },
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
    release_pending(state, actions);
    assert!(actions.len() <= ACTIONS_MAX);
}

/// Releases late grants of given-up claims, and forgets those whose grant has lapsed.
fn release_pending(state: &mut State, actions: &mut Vec<Action>) {
    let now = state.now;
    let lease = state.config.lease_millis;
    let up = state.board == BoardHealth::Up;
    state
        .pending_claims
        .retain(|pending| now.0 < pending.since.0.saturating_add(lease));
    for pending in state.pending_claims.iter_mut() {
        let Some(epoch) = pending.epoch else {
            continue;
        };
        if up && !pending.releasing && now >= pending.retry_at {
            pending.releasing = true;
            let (run, task) = (pending.run, pending.task);
            actions.push(Action::Release { run, task, epoch });
        }
    }
    assert!(state.pending_claims.len() <= PENDING_CLAIMS_MAX);
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
        if state.runs.len() >= agents_max || unanswered >= PENDING_CLAIMS_MAX {
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
    }
    assert!(state.runs.len() <= agents_max);
}

/// The oldest claimable task, its role, and the lowest free slot of that role.
fn next_claimable(state: &State) -> Option<(usize, usize, u8)> {
    let mut best: Option<(usize, usize, u8)> = None;
    for (index, task) in state.tasks.iter().enumerate() {
        let Some(role) = task.role else {
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
}

fn phase_expired(state: &mut State, index: usize, actions: &mut Vec<Action>) {
    let now = state.now;
    let entry = &mut state.runs[index];
    let (run, task) = (entry.id, entry.task);
    match entry.phase {
        // Whether the board granted it is unknown; a late grant is released.
        Phase::Claiming => {
            assert!(state.pending_claims.len() < PENDING_CLAIMS_MAX);
            let since = entry.phase_start;
            state.pending_claims.push(PendingClaim {
                run,
                task,
                since,
                epoch: None,
                releasing: false,
                retry_at: now,
            });
            remove_run(state, index);
        }
        Phase::Preparing | Phase::Judging => {
            begin_settle(state, index, Settlement::Release, actions);
        }
        Phase::Running => {
            set_phase(entry, Phase::Stopping(StopReason::TimeLimit), now);
            send_stop(entry, actions);
        }
        // The closing write is abandoned and the lease lapses, as when its tries run out.
        Phase::Settling => remove_run(state, index),
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
    actions.push(Action::Exit(0));
    assert!(state.runs.is_empty());
}
