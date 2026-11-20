//! The daemon's decisions for workers: one pure `step` from a state and an event to actions.

use crate::board::{TaskId, TaskState, TaskView, Timestamp};
use crate::feed::{BoardEvent, Cursor, EventKind};
use crate::roles::{AGENTS_MAX, RoleSpec};
use crate::settle::Verdict;

/// Upper bound on the open tasks the core tracks.
pub const TASKS_MAX: usize = 256;
/// Upper bound on the roles in the table the core is given.
pub const ROLES_MAX: usize = 16;
/// Upper bound on the actions one step emits: a few per run, plus a few of its own.
pub const ACTIONS_MAX: usize = 4 * AGENTS_MAX as usize + 8;
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
        }
    }

    fn assert_valid(&self) {
        assert!(!self.roles.is_empty() && self.roles.len() <= ROLES_MAX);
        assert!(self.roles.iter().all(|role| role.slots > 0));
        assert!(self.agents_max > 0 && self.agents_max <= AGENTS_MAX);
        assert!(self.tasks_max > 0 && self.tasks_max <= TASKS_MAX);
        assert!(self.lease_millis >= 3 && self.attempt_millis_max > 0);
        assert!(self.settle_tries_max > 0);
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
    pub branch: Option<String>,
    pub renewing: bool,
    /// The closing write, once decided; it is retried until it succeeds or runs out.
    pub settlement: Option<Settlement>,
    pub settle_tries: u8,
    /// A closing write is in flight.
    pub settling: bool,
    pub retry_at: Timestamp,
    /// The agent process exists and has not been seen to exit.
    pub agent_alive: bool,
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
    Prepared(RunId),
    PrepareBlocked {
        run: RunId,
        kind: String,
        reason: String,
    },
    AgentExited(RunId),
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
    untracked: usize,
    runs: Vec<Run>,
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
            untracked: 0,
            runs: Vec::new(),
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
    pub fn untracked(&self) -> usize {
        self.untracked
    }
    pub fn runs(&self) -> &[Run] {
        &self.runs
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
            if let Some(index) = run_in(state, run, Phase::Claiming) {
                remove_run(state, index);
            }
        }
        Event::Prepared(run) => on_prepared(state, run, actions),
        Event::PrepareBlocked { run, kind, reason } => {
            if let Some(index) = run_in(state, run, Phase::Preparing) {
                begin_settle(state, index, Settlement::Block { kind, reason }, actions);
            }
        }
        Event::AgentExited(run) => on_agent_exited(state, run, actions),
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
            if let Some(index) = state.run_index(run) {
                abandon(state, index, actions);
            }
        }
        Event::Settled(run) => {
            state.board = BoardHealth::Up;
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
        None if state.tasks.len() < state.config.tasks_max => state.tasks.push(task),
        None => state.untracked = state.untracked.saturating_add(1),
    }
}

fn reconcile(state: &mut State, views: &[TaskView]) {
    state.tasks.clear();
    state.untracked = 0;
    for view in views {
        if is_closed(view.state) {
            continue;
        }
        if state.tasks.len() == state.config.tasks_max {
            state.untracked = state.untracked.saturating_add(1);
            continue;
        }
        state.tasks.push(Task {
            id: view.id,
            state: view.state,
            role: role_of(&state.config, &view.labels),
            branch: label(&view.labels, "branch"),
        });
    }
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
    let now = state.now;
    let pause = state.config.settle_pause_millis;
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
    let Some(index) = run_in(state, run, Phase::Claiming) else {
        return;
    };
    let now = state.now;
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

fn on_prepared(state: &mut State, run: RunId, actions: &mut Vec<Action>) {
    let Some(index) = run_in(state, run, Phase::Preparing) else {
        return;
    };
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
    set_phase(entry, Phase::Abandoning, now);
    if !already_stopping {
        let (run, task) = (entry.id, entry.task);
        actions.push(Action::StopAgent { run, task });
    }
}

fn stop_now(state: &mut State, actions: &mut Vec<Action>) {
    assert_eq!(state.stopping, Stopping::Now);
    let now = state.now;
    for entry in state.runs.iter_mut() {
        if entry.phase == Phase::Running {
            set_phase(entry, Phase::Stopping(StopReason::StopNow), now);
            let (run, task) = (entry.id, entry.task);
            actions.push(Action::StopAgent { run, task });
        }
    }
    assert!(state.runs.iter().all(|run| run.phase != Phase::Running));
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
    let attempt_max = state.config.attempt_millis_max;
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
        if entry.phase == Phase::Running
            && now.0 >= entry.attempt_start.0.saturating_add(attempt_max)
        {
            set_phase(entry, Phase::Stopping(StopReason::TimeLimit), now);
            let (run, task) = (entry.id, entry.task);
            actions.push(Action::StopAgent { run, task });
        }
        if entry.phase == Phase::Settling {
            try_settle(state, index, actions);
        }
    }
    assert!(actions.len() <= ACTIONS_MAX);
}

fn claim_ready(state: &mut State, actions: &mut Vec<Action>) {
    if state.board != BoardHealth::Up || state.stopping != Stopping::No {
        return;
    }
    let agents_max = usize::from(state.config.agents_max);
    while state.runs.len() < agents_max {
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
            branch,
            renewing: false,
            settlement: None,
            settle_tries: 0,
            settling: false,
            retry_at: now,
            agent_alive: false,
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

fn finish_stopping(state: &mut State, actions: &mut Vec<Action>) {
    if state.stopping == Stopping::No || !state.runs.is_empty() || state.exited {
        return;
    }
    state.exited = true;
    actions.push(Action::Exit(0));
    assert!(state.runs.is_empty());
}
