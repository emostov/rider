//! The simulated world: a fake board, git, process table and clock around one core.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use rider::board::{TaskId, TaskState, TaskView, Timestamp};
use rider::core::{
    ACTIONS_MAX, Action, BoardHealth, Call, Config, Event, Phase, RunId, Settlement, State,
    StopReason, Stopping, Untracked, step,
};
use rider::feed::{BoardEvent, Cursor, EventKind};
use rider::roles::ROLES;
use rider::settle::Verdict;

/// Steps with faults and new work; after them the world drains and stops the core.
const STEPS_MAX: usize = 1500;
/// Steps allowed for the core to stop once it has been signalled.
const STOP_STEPS_MAX: usize = 3000;
/// Upper bound on tasks the fake board holds.
const BOARD_TASKS_MAX: usize = 40;
const SECOND: u64 = 1000;
const MINUTE: u64 = 60 * SECOND;
/// The longest pause between steps while a result is pending.
const RESULT_PAUSE_MAX: u64 = 20 * SECOND;
const BRANCHES: [&str; 3] = ["stack-a", "stack-b", "stack-c"];
/// One seed in this many tracks few tasks, so the untracked count is exercised.
const SMALL_TASKS_EVERY: u64 = 4;
/// The task bound those seeds run under.
const SMALL_TASKS_MAX: usize = 6;

struct FakeTask {
    id: TaskId,
    state: TaskState,
    labels: Vec<(String, String)>,
    epoch: u64,
    holder: Option<RunId>,
    lease_until: Timestamp,
}

struct Agent {
    run: RunId,
    /// The id of the `Spawn` the agent's exit answers.
    spawn: u64,
    exit_at: Timestamp,
    /// The agent ignores every stop and exits only at `exit_at`.
    stubborn: bool,
}

/// What the checker remembers about one run.
#[derive(Default)]
struct RunLog {
    claimed: bool,
    prepared: bool,
    /// The branch `Prepared` reported: the one the run's agent works on.
    branch: Option<String>,
    lost: bool,
    /// `Judged` results, and the verdict of the last.
    judged: u32,
    verdict: Option<Verdict>,
    /// `PrepareBlocked` was delivered: the run may be settled as blocked without a judgment.
    blocked: bool,
    /// The core stopped the agent at its time limit: it may be failed without a judgment.
    time_limited: bool,
    settles: u32,
    settle_failures: u32,
}

pub struct World {
    rng: fastrand::Rng,
    config: Config,
    core: State,
    now: Timestamp,
    tasks: Vec<FakeTask>,
    feed: VecDeque<BoardEvent>,
    cursor: u64,
    /// Results not yet delivered, each with the id of the outstanding action it answers.
    /// A result is not delivered before its time.
    results: Vec<(Event, u64, Timestamp)>,
    agents: Vec<Agent>,
    down_until: Timestamp,
    /// Actions not yet answered, by run (`None` for a listing or a feed restart).
    outstanding: Vec<(u64, Option<RunId>, &'static str)>,
    next_action: u64,
    log: HashMap<RunId, RunLog>,
    settled_epochs: HashSet<(u64, u64)>,
    /// Open tasks as the delivered feed events and snapshots told the core.
    told_open: BTreeSet<u64>,
    /// Runs whose agent has been spawned.
    spawned: HashSet<RunId>,
    /// Whether the core had been told to stop now before the current step.
    stopped_now: bool,
    signals: u8,
    exited: bool,
    pub trace: Vec<String>,
}

impl World {
    pub fn new(seed: u64) -> World {
        let mut config = Config::from_roles(&ROLES);
        if seed % SMALL_TASKS_EVERY == SMALL_TASKS_EVERY - 1 {
            config.tasks_max = SMALL_TASKS_MAX;
        }
        let now = Timestamp(1_800_000_000_000);
        World {
            rng: fastrand::Rng::with_seed(seed),
            core: State::new(config.clone(), now),
            config,
            now,
            tasks: Vec::new(),
            feed: VecDeque::new(),
            cursor: 0,
            results: Vec::new(),
            agents: Vec::new(),
            down_until: now,
            outstanding: Vec::new(),
            next_action: 0,
            log: HashMap::new(),
            settled_epochs: HashSet::new(),
            told_open: BTreeSet::new(),
            spawned: HashSet::new(),
            stopped_now: false,
            signals: 0,
            exited: false,
            trace: Vec::new(),
        }
    }

    pub fn run(&mut self) -> Result<(), String> {
        self.deliver(Event::Tick(self.now))?;
        for index in 0..STEPS_MAX {
            self.turn(true)
                .map_err(|error| format!("step {index}: {error}"))?;
        }
        for index in 0..STOP_STEPS_MAX {
            if self.exited && self.results.is_empty() {
                break;
            }
            if self.signals < 2 && self.rng.u32(0..40) == 0 {
                self.signals += 1;
                self.deliver(Event::Signal)?;
                continue;
            }
            self.turn(false)
                .map_err(|error| format!("stop step {index}: {error}"))?;
        }
        if !self.exited {
            return Err(format!(
                "the core did not exit; runs: {:?}",
                self.core.runs()
            ));
        }
        if !self.outstanding.is_empty() {
            return Err(format!(
                "invariant 10: actions never answered: {:?}",
                self.outstanding
            ));
        }
        let now = self.now;
        let held = self
            .tasks
            .iter()
            .find(|task| task.holder.is_some() && task.lease_until > now);
        match held {
            Some(task) => Err(format!(
                "task {} is still claimed by {:?} until {:?} after the core exited at {now:?}",
                task.id.0, task.holder, task.lease_until
            )),
            None => Ok(()),
        }
    }

    fn turn(&mut self, faults: bool) -> Result<(), String> {
        let choice = self.rng.u32(0..100);
        let now = self.now;
        let ready: Vec<usize> = (0..self.results.len())
            .filter(|index| self.results[*index].2 <= now)
            .collect();
        if choice < 40 && !ready.is_empty() {
            let index = ready[self.rng.usize(0..ready.len())];
            let (event, id, _) = self.results.swap_remove(index);
            self.answer(id, &event)?;
            return self.deliver(event);
        }
        if choice < 60
            && let Some(event) = self.feed.pop_front()
        {
            return self.deliver(Event::Feed(event));
        }
        if choice < 80 || !faults {
            return self.advance();
        }
        match choice {
            80..=87 => self.create_task(),
            88..=89 => self.lose_claim(),
            90..=91 => self.crash_agent(),
            92 => self.down_until = Timestamp(self.now.0 + self.rng.u64(1..10) * MINUTE),
            93 => {
                self.feed.clear();
                let id = self.expect(None, "feed");
                self.reply(id, Event::FeedLost);
            }
            94..=95 => self.unblock(),
            96 => self.cancel_task(),
            _ => return self.advance(),
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<(), String> {
        // A pending result is a board call or a local step, answered within seconds.
        let pause = if !self.results.is_empty() {
            self.rng.u64(SECOND..RESULT_PAUSE_MAX)
        } else if self.rng.u32(0..200) == 0 {
            90 * MINUTE
        } else {
            self.rng.u64(SECOND..3 * MINUTE)
        };
        self.now = Timestamp(self.now.0 + pause);
        let now = self.now;
        for index in 0..self.tasks.len() {
            if self.tasks[index].holder.is_some() && self.tasks[index].lease_until <= now {
                self.expire(index);
            }
        }
        let (done, running): (Vec<Agent>, Vec<Agent>) = self
            .agents
            .drain(..)
            .partition(|agent| agent.exit_at <= now);
        self.agents = running;
        for agent in done {
            self.reply(agent.spawn, Event::AgentExited(agent.run));
        }
        self.deliver(Event::Tick(now))
    }

    fn create_task(&mut self) {
        if self.tasks.len() >= BOARD_TASKS_MAX {
            return;
        }
        let id = TaskId(self.tasks.len() as u64 + 1);
        let role = match self.rng.usize(0..ROLES.len() + 1) {
            index if index < ROLES.len() => ROLES[index].label,
            _ => "unknown",
        };
        let mut labels = vec![("role".to_string(), role.to_string())];
        if self.rng.bool() {
            let branch = BRANCHES[self.rng.usize(0..BRANCHES.len())];
            labels.push(("branch".to_string(), branch.to_string()));
        }
        self.tasks.push(FakeTask {
            id,
            state: TaskState::Ready,
            labels,
            epoch: 0,
            holder: None,
            lease_until: self.now,
        });
        self.publish(self.tasks.len() - 1, EventKind::TaskCreated);
    }

    fn lose_claim(&mut self) {
        let held: Vec<usize> = (0..self.tasks.len())
            .filter(|index| self.tasks[*index].holder.is_some())
            .collect();
        if !held.is_empty() {
            let index = held[self.rng.usize(0..held.len())];
            self.expire(index);
        }
    }

    fn crash_agent(&mut self) {
        if !self.agents.is_empty() {
            let index = self.rng.usize(0..self.agents.len());
            let agent = self.agents.swap_remove(index);
            self.reply(agent.spawn, Event::AgentExited(agent.run));
        }
    }

    fn unblock(&mut self) {
        let index = self
            .tasks
            .iter()
            .position(|task| task.state == TaskState::Blocked);
        if let Some(index) = index {
            self.tasks[index].state = TaskState::Ready;
            self.publish(index, EventKind::TaskUnblocked);
        }
    }

    /// The owner cancels an open task, whoever holds it.
    fn cancel_task(&mut self) {
        let open: Vec<usize> = (0..self.tasks.len())
            .filter(|index| !is_closed(self.tasks[*index].state))
            .collect();
        if !open.is_empty() {
            let index = open[self.rng.usize(0..open.len())];
            self.tasks[index].holder = None;
            self.tasks[index].state = TaskState::Canceled;
            self.publish(index, EventKind::TaskCanceled);
        }
    }

    fn expire(&mut self, index: usize) {
        self.tasks[index].holder = None;
        self.tasks[index].state = TaskState::Ready;
        self.publish(index, EventKind::TaskExpired);
    }

    fn publish(&mut self, index: usize, kind: EventKind) {
        self.cursor += 1;
        let task = &self.tasks[index];
        self.feed.push_back(BoardEvent {
            cursor: Cursor(self.cursor),
            kind,
            task: task.id,
            channel: "work/sim".to_string(),
            labels: task.labels.clone(),
            state: Some(task.state),
            actor: None,
        });
    }

    fn deliver(&mut self, event: Event) -> Result<(), String> {
        self.note_result(&event);
        self.note_told(&event);
        self.stopped_now = self.core.stopping() == Stopping::Now;
        let actions = step(&mut self.core, event.clone());
        self.trace.push(format!("{event:?} -> {actions:?}"));
        self.check(&actions)?;
        for action in actions {
            self.perform(action)?;
        }
        Ok(())
    }

    /// Records one emitted action instance; its one result must name the returned id.
    fn expect(&mut self, run: Option<RunId>, kind: &'static str) -> u64 {
        self.next_action += 1;
        self.outstanding.push((self.next_action, run, kind));
        self.next_action
    }

    fn reply(&mut self, id: u64, event: Event) {
        self.results.push((event, id, self.now));
    }

    fn answer(&mut self, id: u64, event: &Event) -> Result<(), String> {
        match self.outstanding.iter().position(|entry| entry.0 == id) {
            Some(index) => {
                self.outstanding.swap_remove(index);
                Ok(())
            }
            None => Err(format!(
                "invariant 10: {event:?} answers action {id}, which is not outstanding"
            )),
        }
    }

    fn note_result(&mut self, event: &Event) {
        let (run, entry) = match event {
            Event::Claimed { run, .. } => (*run, "claimed"),
            Event::Prepared { run, .. } => (*run, "prepared"),
            Event::PrepareBlocked { run, .. } => (*run, "blocked"),
            Event::Judged { run, .. } => (*run, "judged"),
            Event::ClaimLost(run) => (*run, "lost"),
            Event::BoardFailed(Call::Settle(run) | Call::Release(run)) => (*run, "failure"),
            _ => return,
        };
        let log = self.log.entry(run).or_default();
        match (entry, event) {
            ("claimed", _) => log.claimed = true,
            (_, Event::Prepared { branch, .. }) => {
                log.prepared = true;
                log.branch = branch.clone();
            }
            ("blocked", _) => log.blocked = true,
            ("lost", _) => log.lost = true,
            ("failure", _) => log.settle_failures += 1,
            (_, Event::Judged { verdict, .. }) => {
                log.judged += 1;
                log.verdict = Some(verdict.clone());
            }
            _ => panic!("no log entry for {event:?}"),
        }
    }

    fn note_told(&mut self, event: &Event) {
        if self.core.exited() {
            return;
        }
        match event {
            Event::Feed(BoardEvent {
                task,
                state: Some(state),
                ..
            }) => {
                if is_closed(*state) {
                    self.told_open.remove(&task.0);
                } else {
                    self.told_open.insert(task.0);
                }
            }
            Event::Snapshot { tasks, .. } => {
                self.told_open = tasks
                    .iter()
                    .filter(|view| !is_closed(view.state))
                    .map(|view| view.id.0)
                    .collect();
            }
            _ => {}
        }
    }

    fn board_fails(&mut self) -> bool {
        self.now < self.down_until || self.rng.u32(0..20) == 0
    }

    fn perform(&mut self, action: Action) -> Result<(), String> {
        match action {
            Action::Claim { run, task, .. } => self.claim(run, task),
            Action::Prepare { run, task } => {
                let id = self.expect(Some(run), "prepare");
                let branch = self.learn_branch(task);
                let event = match self.rng.u32(0..10) {
                    0 => Event::PrepareBlocked {
                        run,
                        kind: "invalid_brief".to_string(),
                        reason: "base is not a need".to_string(),
                    },
                    _ => Event::Prepared { run, branch },
                };
                self.reply(id, event);
            }
            Action::Spawn { run, .. } => {
                let spawn = self.expect(Some(run), "spawn");
                self.spawned.insert(run);
                let exit_at = Timestamp(self.now.0 + self.rng.u64(MINUTE..70 * MINUTE));
                let stubborn = self.rng.u32(0..6) == 0;
                self.agents.push(Agent {
                    run,
                    spawn,
                    exit_at,
                    stubborn,
                });
            }
            Action::StopAgent { run, .. } => {
                let id = self.expect(Some(run), "stop");
                self.reply(id, Event::StopDone(run));
                let found = self.agents.iter().position(|agent| agent.run == run);
                if let Some(index) = found.filter(|index| !self.agents[*index].stubborn) {
                    let agent = self.agents.swap_remove(index);
                    self.reply(agent.spawn, Event::AgentExited(run));
                }
            }
            Action::Renew { run, task } => self.renew(run, task),
            Action::Judge { run, .. } => {
                let id = self.expect(Some(run), "judge");
                let verdict = match self.rng.u32(0..3) {
                    0 => Verdict::Complete {
                        result: "done".to_string(),
                    },
                    1 => Verdict::Fail {
                        reason: "no commit".to_string(),
                    },
                    _ => Verdict::LeaveBlocked,
                };
                self.reply(id, Event::Judged { run, verdict });
            }
            Action::Settle {
                run,
                task,
                epoch,
                settlement,
            } => self.close(run, task, epoch, Some(settlement))?,
            Action::Release { run, task, epoch } => self.close(run, task, epoch, None)?,
            Action::ListTasks => self.list(),
            Action::RestartFeed(cursor) => {
                let id = self.expect(None, "feed");
                self.reply(id, Event::FeedResumed(cursor));
            }
            Action::Exit(_) => self.exited = true,
        }
        Ok(())
    }

    /// The task's branch as preparation finds it; for a task with none it sometimes picks one,
    /// and an existing one it sometimes moves to another branch.
    fn learn_branch(&mut self, task: TaskId) -> Option<String> {
        let index = self.tasks.iter().position(|fake| fake.id == task)?;
        let labels = &mut self.tasks[index].labels;
        if let Some(found) = labels.iter().position(|(name, _)| name == "branch") {
            if self.rng.u32(0..4) == 0 {
                let branch = BRANCHES[self.rng.usize(0..BRANCHES.len())];
                labels[found].1 = branch.to_string();
                self.publish(index, EventKind::LabelsChanged);
            }
            return Some(self.tasks[index].labels[found].1.clone());
        }
        if self.rng.u32(0..3) != 0 {
            return None;
        }
        let branch = BRANCHES[self.rng.usize(0..BRANCHES.len())];
        let labels = &mut self.tasks[index].labels;
        labels.push(("branch".to_string(), branch.to_string()));
        self.publish(index, EventKind::LabelsChanged);
        Some(branch.to_string())
    }

    fn holds(&self, run: RunId, task: TaskId) -> Option<usize> {
        let index = self.tasks.iter().position(|fake| fake.id == task)?;
        let fake = &self.tasks[index];
        (fake.holder == Some(run) && fake.lease_until > self.now).then_some(index)
    }

    fn claim(&mut self, run: RunId, task: TaskId) {
        let id = self.expect(Some(run), "claim");
        if self.board_fails() {
            self.reply(id, Event::BoardFailed(Call::Claim(run)));
            return;
        }
        let index = self.tasks.iter().position(|fake| fake.id == task);
        let free = index.filter(|index| {
            self.tasks[*index].state == TaskState::Ready && self.tasks[*index].holder.is_none()
        });
        match free {
            Some(index) if self.rng.u32(0..10) != 0 => {
                let until = Timestamp(self.now.0 + self.config.lease_millis);
                let fake = &mut self.tasks[index];
                fake.epoch += 1;
                fake.holder = Some(run);
                fake.state = TaskState::Claimed;
                fake.lease_until = until;
                let epoch = fake.epoch;
                self.publish(index, EventKind::TaskClaimed);
                // Sometimes the grant is answered only after the core has given the claim up.
                let late = self.rng.u32(0..15) == 0;
                let at = Timestamp(self.now.0 + self.config.claim_millis_max + MINUTE);
                let at = if late { at } else { self.now };
                self.results
                    .push((Event::Claimed { run, epoch, until }, id, at));
            }
            _ => self.reply(id, Event::ClaimRefused(run)),
        }
    }

    fn renew(&mut self, run: RunId, task: TaskId) {
        let id = self.expect(Some(run), "renew");
        if self.board_fails() {
            self.reply(id, Event::BoardFailed(Call::Renew(run)));
            return;
        }
        match self.holds(run, task) {
            Some(index) => {
                let until = Timestamp(self.now.0 + self.config.lease_millis);
                self.tasks[index].lease_until = until;
                self.reply(id, Event::Renewed { run, until });
            }
            None => self.reply(id, Event::ClaimLost(run)),
        }
    }

    fn close(
        &mut self,
        run: RunId,
        task: TaskId,
        epoch: u64,
        settlement: Option<Settlement>,
    ) -> Result<(), String> {
        let kind = if settlement.is_some() {
            "settle"
        } else {
            "release"
        };
        let id = self.expect(Some(run), kind);
        if self.board_fails() {
            let call = if settlement.is_some() {
                Call::Settle(run)
            } else {
                Call::Release(run)
            };
            self.reply(id, Event::BoardFailed(call));
            return Ok(());
        }
        let Some(index) = self.holds(run, task) else {
            self.reply(id, Event::ClaimLost(run));
            return Ok(());
        };
        if !self.settled_epochs.insert((task.0, epoch)) {
            return Err(format!(
                "task {} was settled twice in epoch {epoch}",
                task.0
            ));
        }
        let (state, event_kind) = match settlement {
            None => (TaskState::Ready, EventKind::TaskReleased),
            Some(Settlement::Verdict(Verdict::Complete { .. })) => {
                (TaskState::Completed, EventKind::TaskCompleted)
            }
            Some(Settlement::Verdict(Verdict::Fail { .. })) => {
                (TaskState::Failed, EventKind::TaskFailed)
            }
            Some(_) => (TaskState::Blocked, EventKind::TaskBlocked),
        };
        self.tasks[index].holder = None;
        self.tasks[index].state = state;
        self.publish(index, event_kind);
        self.reply(id, Event::Settled(run));
        Ok(())
    }

    fn list(&mut self) {
        let id = self.expect(None, "list");
        if self.board_fails() {
            self.reply(id, Event::BoardFailed(Call::ListTasks));
            return;
        }
        let tasks = self
            .tasks
            .iter()
            .map(|fake| TaskView {
                id: fake.id,
                text: String::new(),
                state: fake.state,
                labels: fake.labels.clone(),
                needs: Vec::new(),
                epoch: fake.epoch,
                claim: None,
            })
            .collect();
        let cursor = Cursor(self.cursor);
        self.reply(id, Event::Snapshot { tasks, cursor });
    }

    fn check(&mut self, actions: &[Action]) -> Result<(), String> {
        let runs = self.core.runs();
        if runs.len() > usize::from(self.config.agents_max) {
            return Err(format!("invariant 1: {} runs", runs.len()));
        }
        for (role, spec) in self.config.roles.iter().enumerate() {
            let count = runs.iter().filter(|run| run.role == role).count();
            if count > usize::from(spec.slots) {
                return Err(format!("invariant 1: {count} runs of role {}", spec.label));
            }
        }
        for (index, run) in runs.iter().enumerate() {
            for other in &runs[index + 1..] {
                if run.task == other.task {
                    return Err(format!("invariant 2: {run:?} and {other:?}"));
                }
            }
            if run.phase == Phase::Stopping(StopReason::TimeLimit) {
                self.log.entry(run.id).or_default().time_limited = true;
            }
            let now = self.core.now();
            let covered = (self.outstanding.iter())
                .any(|entry| entry.1 == Some(run.id) && matches!(entry.2, "renew" | "stop"))
                || actions.iter().any(|action| {
                    matches!(action, Action::Renew { run: id, .. }
                        | Action::StopAgent { run: id, .. } if *id == run.id)
                });
            if run.phase == Phase::Running && run.lease_until <= now && !covered {
                return Err(format!(
                    "invariant 3: {run:?} runs on a lapsed lease at {now:?}"
                ));
            }
            // A lost agent has ended its phase: the run waits only for the agent's exit.
            let limit = run.phase_start.0 + self.config.phase_millis_max(run.phase);
            if now.0 >= limit && !run.agent_lost {
                return Err(format!(
                    "invariant 10: {run:?} is past its phase's time limit"
                ));
            }
        }
        self.check_agents_known()?;
        self.check_branches()?;
        let tracked = self.core.tasks().len();
        let untracked = self.core.untracked();
        let open = self.told_open.len();
        let counted = match untracked {
            Untracked::Exactly(count) => tracked + count == open,
            Untracked::AtLeast(count) => tracked + count <= open,
        };
        if !self.core.exited() && !counted {
            return Err(format!(
                "invariant 8: {tracked} tracked and {untracked:?}, but {open} open tasks were told"
            ));
        }
        if actions.len() > ACTIONS_MAX
            || self.core.tasks().len() > self.config.tasks_max
            || self.outstanding.len() > 4096
        {
            return Err("invariant 8: a collection is over its bound".to_string());
        }
        self.check_actions(actions)
    }

    fn check_actions(&mut self, actions: &[Action]) -> Result<(), String> {
        let blocked = self.core.board() != BoardHealth::Up || self.core.stopping() != Stopping::No;
        for action in actions {
            let (run, write) = match action {
                Action::Claim { run, .. } => {
                    if blocked {
                        return Err(format!("invariant 9: {action:?} while down or stopping"));
                    }
                    (*run, true)
                }
                Action::Renew { run, .. } | Action::Release { run, .. } => (*run, true),
                Action::Settle {
                    run, settlement, ..
                } => {
                    if self.stopped_now {
                        return Err(format!("stop-now: {action:?} counts an attempt"));
                    }
                    let log = self.log.entry(*run).or_default();
                    log.settles += 1;
                    let judged = log.verdict.clone().map(Settlement::Verdict);
                    let based = match settlement {
                        _ if log.judged > 0 => {
                            log.judged == 1 && judged.as_ref() == Some(settlement)
                        }
                        Settlement::Block { .. } => log.blocked,
                        Settlement::Verdict(Verdict::Fail { .. }) => log.time_limited,
                        _ => false,
                    };
                    if !based || log.settles > 1 + log.settle_failures {
                        return Err(format!("invariant 6: {action:?} without one judgment"));
                    }
                    (*run, true)
                }
                Action::Spawn { run, .. } => {
                    let log = self.log.entry(*run).or_default();
                    if !log.claimed || !log.prepared {
                        return Err(format!("invariant 5: {action:?} before claim and prepare"));
                    }
                    (*run, false)
                }
                _ => continue,
            };
            if write && self.log.get(&run).is_some_and(|log| log.lost) {
                return Err(format!("invariant 4: {action:?} after its claim was lost"));
            }
        }
        Ok(())
    }

    /// A run whose agent still runs is never treated as if the agent had exited.
    fn check_agents_known(&self) -> Result<(), String> {
        for agent in &self.agents {
            let run = self.core.runs().iter().find(|run| run.id == agent.run);
            if !run.is_some_and(|run| run.agent_alive) {
                return Err(format!(
                    "{:?}'s agent still runs, but the core has {run:?}",
                    agent.run
                ));
            }
        }
        Ok(())
    }

    /// Invariant 2 on branches: no two runs reserve one, and no two spawned runs work on one.
    fn check_branches(&self) -> Result<(), String> {
        let runs = self.core.runs();
        for (index, run) in runs.iter().enumerate() {
            let other = (runs[index + 1..].iter())
                .find(|other| run.branch.is_some() && other.branch == run.branch);
            if let Some(other) = other {
                return Err(format!(
                    "invariant 2: {run:?} and {other:?} reserve one branch"
                ));
            }
        }
        let spawned: Vec<_> = (self.core.runs().iter())
            .filter(|run| self.spawned.contains(&run.id))
            .filter_map(|run| Some((run, self.log.get(&run.id)?.branch.as_deref()?)))
            .collect();
        for (index, (run, branch)) in spawned.iter().enumerate() {
            if let Some((other, _)) = spawned[index + 1..].iter().find(|(_, b)| b == branch) {
                return Err(format!(
                    "invariant 2: {run:?} and {other:?} both on {branch}"
                ));
            }
        }
        Ok(())
    }
}

fn is_closed(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled
    )
}
