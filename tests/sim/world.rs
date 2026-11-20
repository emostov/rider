//! The simulated world: a fake board, git, process table and clock around one core.

use std::collections::{HashMap, HashSet, VecDeque};

use rider::board::{TaskId, TaskState, TaskView, Timestamp};
use rider::core::{
    ACTIONS_MAX, Action, BoardHealth, Call, Config, Event, Phase, RunId, Settlement, State,
    Stopping, step,
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
const BRANCHES: [&str; 3] = ["stack-a", "stack-b", "stack-c"];

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
    exit_at: Timestamp,
}

/// What the checker remembers about one run.
#[derive(Default)]
struct RunLog {
    claimed: bool,
    prepared: bool,
    lost: bool,
    /// Results a `Settle` may follow: `Judged`, `PrepareBlocked`, or a stopped agent's exit.
    settle_bases: u32,
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
    /// Results not yet delivered, each with the outstanding action it answers.
    results: Vec<(Event, Option<RunId>, &'static str)>,
    agents: Vec<Agent>,
    down_until: Timestamp,
    /// Actions not yet answered, by run (`None` for a listing or a feed restart).
    outstanding: Vec<(Option<RunId>, &'static str)>,
    log: HashMap<RunId, RunLog>,
    settled_epochs: HashSet<(u64, u64)>,
    signals: u8,
    exited: bool,
    pub trace: Vec<String>,
}

impl World {
    pub fn new(seed: u64) -> World {
        let config = Config::from_roles(&ROLES);
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
            log: HashMap::new(),
            settled_epochs: HashSet::new(),
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
        Ok(())
    }

    fn turn(&mut self, faults: bool) -> Result<(), String> {
        let choice = self.rng.u32(0..100);
        if choice < 40 && !self.results.is_empty() {
            let index = self.rng.usize(0..self.results.len());
            let (event, run, kind) = self.results.swap_remove(index);
            self.answer(run, kind)?;
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
                self.outstanding.push((None, "feed"));
                self.reply(None, "feed", Event::FeedLost);
            }
            94..=95 => self.unblock(),
            _ => return self.advance(),
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<(), String> {
        let jump = if self.rng.u32(0..200) == 0 {
            90 * MINUTE
        } else {
            0
        };
        self.now = Timestamp(self.now.0 + self.rng.u64(SECOND..3 * MINUTE) + jump);
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
            self.reply(Some(agent.run), "spawn", Event::AgentExited(agent.run));
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
            self.reply(Some(agent.run), "spawn", Event::AgentExited(agent.run));
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
        let actions = step(&mut self.core, event.clone());
        self.trace.push(format!("{event:?} -> {actions:?}"));
        self.check(&actions)?;
        for action in actions {
            self.perform(action)?;
        }
        Ok(())
    }

    fn reply(&mut self, run: Option<RunId>, kind: &'static str, event: Event) {
        self.results.push((event, run, kind));
    }

    fn answer(&mut self, run: Option<RunId>, kind: &'static str) -> Result<(), String> {
        if kind == "spawn" {
            self.outstanding.retain(|entry| *entry != (run, "stop"));
        }
        match self
            .outstanding
            .iter()
            .position(|entry| *entry == (run, kind))
        {
            Some(index) => {
                self.outstanding.swap_remove(index);
                Ok(())
            }
            None => Err(format!(
                "a {kind} result for {run:?} answers no outstanding action: {:?}",
                self.outstanding
            )),
        }
    }

    fn note_result(&mut self, event: &Event) {
        let (run, entry) = match event {
            Event::Claimed { run, .. } => (*run, "claimed"),
            Event::Prepared(run) => (*run, "prepared"),
            Event::PrepareBlocked { run, .. } | Event::AgentExited(run) => (*run, "basis"),
            Event::Judged { run, .. } => (*run, "basis"),
            Event::ClaimLost(run) => (*run, "lost"),
            Event::BoardFailed(Call::Settle(run) | Call::Release(run)) => (*run, "failure"),
            _ => return,
        };
        let log = self.log.entry(run).or_default();
        match entry {
            "claimed" => log.claimed = true,
            "prepared" => log.prepared = true,
            "lost" => log.lost = true,
            "failure" => log.settle_failures += 1,
            _ => log.settle_bases += 1,
        }
    }

    fn board_fails(&mut self) -> bool {
        self.now < self.down_until || self.rng.u32(0..20) == 0
    }

    fn perform(&mut self, action: Action) -> Result<(), String> {
        match action {
            Action::Claim { run, task, .. } => self.claim(run, task),
            Action::Prepare { run, .. } => {
                self.outstanding.push((Some(run), "prepare"));
                let event = match self.rng.u32(0..10) {
                    0 => Event::PrepareBlocked {
                        run,
                        kind: "invalid_brief".to_string(),
                        reason: "base is not a need".to_string(),
                    },
                    _ => Event::Prepared(run),
                };
                self.reply(Some(run), "prepare", event);
            }
            Action::Spawn { run, .. } => {
                self.outstanding.push((Some(run), "spawn"));
                let exit_at = Timestamp(self.now.0 + self.rng.u64(MINUTE..70 * MINUTE));
                self.agents.push(Agent { run, exit_at });
            }
            Action::StopAgent { run, .. } => {
                self.outstanding.push((Some(run), "stop"));
                if let Some(index) = self.agents.iter().position(|agent| agent.run == run) {
                    self.agents.swap_remove(index);
                    self.reply(Some(run), "spawn", Event::AgentExited(run));
                }
            }
            Action::Renew { run, task } => self.renew(run, task),
            Action::Judge { run, .. } => {
                self.outstanding.push((Some(run), "judge"));
                let verdict = match self.rng.u32(0..3) {
                    0 => Verdict::Complete {
                        result: "done".to_string(),
                    },
                    1 => Verdict::Fail {
                        reason: "no commit".to_string(),
                    },
                    _ => Verdict::LeaveBlocked,
                };
                self.reply(Some(run), "judge", Event::Judged { run, verdict });
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
                self.outstanding.push((None, "feed"));
                self.reply(None, "feed", Event::FeedResumed(cursor));
            }
            Action::Exit(_) => self.exited = true,
        }
        Ok(())
    }

    fn holds(&self, run: RunId, task: TaskId) -> Option<usize> {
        let index = self.tasks.iter().position(|fake| fake.id == task)?;
        let fake = &self.tasks[index];
        (fake.holder == Some(run) && fake.lease_until > self.now).then_some(index)
    }

    fn claim(&mut self, run: RunId, task: TaskId) {
        self.outstanding.push((Some(run), "claim"));
        if self.board_fails() {
            self.reply(Some(run), "claim", Event::BoardFailed(Call::Claim(run)));
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
                self.reply(Some(run), "claim", Event::Claimed { run, epoch, until });
            }
            _ => self.reply(Some(run), "claim", Event::ClaimRefused(run)),
        }
    }

    fn renew(&mut self, run: RunId, task: TaskId) {
        self.outstanding.push((Some(run), "renew"));
        if self.board_fails() {
            self.reply(Some(run), "renew", Event::BoardFailed(Call::Renew(run)));
            return;
        }
        match self.holds(run, task) {
            Some(index) => {
                let until = Timestamp(self.now.0 + self.config.lease_millis);
                self.tasks[index].lease_until = until;
                self.reply(Some(run), "renew", Event::Renewed { run, until });
            }
            None => self.reply(Some(run), "renew", Event::ClaimLost(run)),
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
        self.outstanding.push((Some(run), kind));
        if self.board_fails() {
            let call = if settlement.is_some() {
                Call::Settle(run)
            } else {
                Call::Release(run)
            };
            self.reply(Some(run), kind, Event::BoardFailed(call));
            return Ok(());
        }
        let Some(index) = self.holds(run, task) else {
            self.reply(Some(run), kind, Event::ClaimLost(run));
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
        self.reply(Some(run), kind, Event::Settled(run));
        Ok(())
    }

    fn list(&mut self) {
        self.outstanding.push((None, "list"));
        if self.board_fails() {
            self.reply(None, "list", Event::BoardFailed(Call::ListTasks));
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
        self.reply(None, "list", Event::Snapshot { tasks, cursor });
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
                let same_branch = run.branch.is_some() && run.branch == other.branch;
                if run.task == other.task || same_branch {
                    return Err(format!("invariant 2: {run:?} and {other:?}"));
                }
            }
            let now = self.core.now();
            let covered = self.outstanding.contains(&(Some(run.id), "renew"))
                || self.outstanding.contains(&(Some(run.id), "stop"))
                || actions.iter().any(|action| {
                    matches!(action, Action::Renew { run: id, .. }
                        | Action::StopAgent { run: id, .. } if *id == run.id)
                });
            if run.phase == Phase::Running && run.lease_until <= now && !covered {
                return Err(format!(
                    "invariant 3: {run:?} runs on a lapsed lease at {now:?}"
                ));
            }
            let limit = run.attempt_start.0 + self.config.attempt_millis_max;
            if run.phase == Phase::Running && now.0 >= limit {
                return Err(format!("invariant 10: {run:?} runs past its time limit"));
            }
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
                Action::Settle { run, .. } => {
                    let log = self.log.entry(*run).or_default();
                    log.settles += 1;
                    if log.settle_bases == 0 || log.settles > 1 + log.settle_failures {
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
}
