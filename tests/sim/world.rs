//! The simulated world: a fake board, git, process table and clock around one core.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use rider::board::{Claim, ListedTask, TaskId, TaskState, TaskView, Timestamp};
use rider::core::{
    ACTIONS_MAX, Action, AgentRecord, BOARD_RESTARTS_MAX, BOARD_UPTIME_MILLIS_MIN, BoardHealth,
    Call, ClientId, Config, Event, FEED_RESTARTS_MAX, FEED_RETRY_MILLIS, FEED_UPTIME_MILLIS_MIN,
    KILLED_REASON, Phase, PidFile, Reply, Request, RunId, Settlement, State, StopReason, Stopping,
    Unaccounted, Untracked, recover, step,
};
use rider::feed::{BoardEvent, Cursor, EventKind};
use rider::roles::ROLES;
use rider::settle::Verdict;

/// Steps with faults and new work; after them the world drains and stops the core.
const STEPS_MAX: usize = 1500;
/// Steps allowed for the core to stop once it has been signalled.
const STOP_STEPS_MAX: usize = 3000;
/// The socket client that kills agents and follows logs.
const OWNER: ClientId = ClientId(3);
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
/// Seeds that track every task cycle the builders limit through 1 to this, so it often binds.
const BUILDERS_CYCLE: u64 = 3;
/// How far before the requested cursor a restarted feed may start replaying.
const REPLAY_BACK_MAX: u64 = 8;
/// The open tasks one listing returns; the rest it only counts, as the board's does past
/// `TASKS_MAX`.
const LISTING_TASKS_MAX: usize = 24;
/// How many events may happen between a listing reading its cursor and reading its pages.
const LISTING_LAG_MAX: u64 = 4;
/// One flapping fault in this many is a burst of feeds that exit at once.
const FLAP_EVERY: u32 = 4;
/// One seed in this many runs a board the core's daemon started, which may die.
const OWNED_BOARD_EVERY: u64 = 2;
/// One restart of a dead board in this many fails at once.
const BOARD_RESTART_FAILS_EVERY: u32 = 2;
/// Upper bound, in minutes, on how long a board left dead stays so until the owner starts it.
const BOARD_DEAD_MINUTES_MAX: u64 = 12;
/// Upper bound on how far past one lease a clock jump lands.
const JUMP_PAST_LEASE_MAX: u64 = 30 * MINUTE;
const WORK_CHANNEL: &str = "work/sim";
const QUEUE_CHANNEL: &str = "queue";
const QUEUED_EVERY: u32 = 5;

/// The newest version of one task the core was told, by cursor.
#[derive(Debug)]
struct Told {
    cursor: u64,
    state: TaskState,
    labels: Vec<(String, String)>,
}

struct FakeTask {
    id: TaskId,
    channel: &'static str,
    state: TaskState,
    labels: Vec<(String, String)>,
    epoch: u64,
    holder: Option<RunId>,
    /// The restarts of the core before the holder's claim was granted or renewed, which tells
    /// apart runs of different cores that share an id.
    generation: u64,
    /// The profile the claim is held as.
    profile: String,
    lease_until: Timestamp,
    /// The cursor of the newest event published about the task.
    version: u64,
}

struct Agent {
    run: RunId,
    task: TaskId,
    /// The agent's process group, and when its leader started, in seconds.
    group: i32,
    start: u64,
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
    /// The role `Prepared` reported: the one the run's agent launches with.
    launch_role: Option<String>,
    /// The task the run claimed.
    task: Option<TaskId>,
    lost: bool,
    /// `Judged` results, and the verdict of the last.
    judged: u32,
    verdict: Option<Verdict>,
    /// `PrepareBlocked` was delivered: the run may be settled as blocked without a judgment.
    blocked: bool,
    /// The core stopped the agent at its time limit: it may be failed without a judgment.
    time_limited: bool,
    /// The owner killed the agent: it may be failed as killed without a judgment.
    killed: bool,
    settles: u32,
    settle_failures: u32,
    /// The profile the run's claim was sent as.
    profile: Option<String>,
}

pub struct World {
    rng: fastrand::Rng,
    /// Draws which tasks work `onto` another, apart from `rng` so the other draws stay put.
    onto_rng: fastrand::Rng,
    queue_rng: fastrand::Rng,
    config: Config,
    core: State,
    now: Timestamp,
    tasks: Vec<FakeTask>,
    feed: VecDeque<BoardEvent>,
    /// Every event the board published, so a restarted feed can replay them.
    history: Vec<BoardEvent>,
    /// The feed is running: published events reach it.
    feed_up: bool,
    /// Restarted feeds that are still to exit at once.
    flaps_left: u32,
    /// Feed losses in a row, by the rule the core must follow.
    feed_restarts: u8,
    feed_since: Timestamp,
    feed_delivered: bool,
    /// Since when feed restarts are exhausted, by the rule the core must follow.
    feed_failed: Option<Timestamp>,
    /// When the last feed tried after exhaustion ended, or `None` while it runs.
    feed_retry_after: Option<Timestamp>,
    /// The core stopped restarting the feed after `FEED_RESTARTS_MAX` losses in a row.
    pub feed_given_up: bool,
    /// A feed tried after exhaustion worked and the core claimed again.
    pub feed_recovered: bool,
    cursor: u64,
    /// Results not yet delivered, each with the id of the outstanding action it answers.
    /// A result is not delivered before its time.
    results: Vec<(Event, u64, Timestamp)>,
    agents: Vec<Agent>,
    /// Agents of the core before the last restart that its recovery left running.
    leftovers: Vec<Agent>,
    next_group: i32,
    down_until: Timestamp,
    /// The daemon started the board: it may die, and the core restarts it.
    owns_board: bool,
    /// The board process is dead until then, unless a restart starts it sooner.
    dead_until: Timestamp,
    /// Board restarts in a row, by the rule the core must follow.
    board_restarts: u8,
    board_since: Option<Timestamp>,
    /// The core left the board dead after `BOARD_RESTARTS_MAX` restarts in a row.
    pub board_given_up: bool,
    /// Every restart of the dead board fails until then.
    spawn_fails_until: Timestamp,
    /// The core gave up on a board whose restarts all failed after it had stayed up long.
    pub spawn_given_up: bool,
    /// Actions not yet answered, by run (`None` for a listing or a feed restart).
    outstanding: Vec<(u64, Option<RunId>, &'static str)>,
    next_action: u64,
    log: HashMap<RunId, RunLog>,
    settled_epochs: HashSet<(u64, u64)>,
    /// The newest version of each task the delivered feed events and snapshots told the core.
    told: HashMap<u64, Told>,
    /// Open tasks the last delivered listing left out and the core has not been told of since.
    left_out: HashSet<u64>,
    /// The newest cursor a delivered listing read: every event up to it is reflected.
    listed_through: u64,
    /// What each listing not yet delivered left out, by the id of its action.
    listings: HashMap<u64, Vec<u64>>,
    /// Tasks whose claim was refused, with no newer news of them nor a tick since.
    refused: HashSet<u64>,
    /// Runs whose agent has been spawned.
    spawned: HashSet<RunId>,
    /// Whether the core had been told to stop now before the current step.
    stopped_now: bool,
    signals: u8,
    exited: bool,
    pub trace: Vec<String>,
    /// The phases runs were in when the core was restarted, by name.
    pub restarted_in: BTreeSet<String>,
    /// The phases runs were in when a board outage began, by name.
    pub down_in: BTreeSet<String>,
    /// The phases runs were in when the clock jumped past every lease, by name.
    pub jumped_in: BTreeSet<String>,
    /// How many times the core was restarted.
    generation: u64,
    /// The replies the core sent `OWNER` during its current request.
    replies: Vec<(ClientId, Reply)>,
}

impl World {
    pub fn new(seed: u64) -> World {
        let mut config = Config::from_roles(&ROLES);
        if seed % SMALL_TASKS_EVERY == SMALL_TASKS_EVERY - 1 {
            config.tasks_max = SMALL_TASKS_MAX;
        } else {
            config.builders_max = u8::try_from(seed % BUILDERS_CYCLE + 1).expect("a small limit");
        }
        let now = Timestamp(1_800_000_000_000);
        World {
            rng: fastrand::Rng::with_seed(seed),
            onto_rng: fastrand::Rng::with_seed(!seed),
            queue_rng: fastrand::Rng::with_seed(seed.rotate_left(32)),
            core: State::new(config.clone(), now),
            config,
            now,
            tasks: Vec::new(),
            feed: VecDeque::new(),
            history: Vec::new(),
            feed_up: true,
            flaps_left: 0,
            feed_restarts: 0,
            feed_since: now,
            feed_delivered: false,
            feed_failed: None,
            feed_retry_after: None,
            feed_given_up: false,
            feed_recovered: false,
            cursor: 0,
            results: Vec::new(),
            agents: Vec::new(),
            leftovers: Vec::new(),
            next_group: 2,
            down_until: now,
            owns_board: seed.is_multiple_of(OWNED_BOARD_EVERY),
            dead_until: now,
            board_restarts: 0,
            board_since: Some(now),
            board_given_up: false,
            spawn_fails_until: now,
            spawn_given_up: false,
            outstanding: Vec::new(),
            next_action: 0,
            log: HashMap::new(),
            settled_epochs: HashSet::new(),
            told: HashMap::new(),
            refused: HashSet::new(),
            left_out: HashSet::new(),
            listings: HashMap::new(),
            listed_through: 0,
            spawned: HashSet::new(),
            stopped_now: false,
            signals: 0,
            exited: false,
            trace: Vec::new(),
            restarted_in: BTreeSet::new(),
            down_in: BTreeSet::new(),
            jumped_in: BTreeSet::new(),
            generation: 0,
            replies: Vec::new(),
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
                // A stop comes as a signal or as `down` on the socket, the second stopping now.
                let event = match self.rng.bool() {
                    true => Event::Signal,
                    false => Event::Client {
                        client: ClientId(1),
                        request: Request::Down {
                            now: self.signals == 2,
                        },
                    },
                };
                self.deliver(event)?;
                continue;
            }
            if self.rng.u32(0..20) == 0 {
                let request = match self.rng.bool() {
                    true => Request::Status,
                    false => Request::Agents,
                };
                let client = ClientId(2);
                self.deliver(Event::Client { client, request })?;
                self.deliver(Event::ClientGone(client))?;
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
        let held = self.tasks.iter().find(|task| {
            task.holder.is_some() && task.generation == self.generation && task.lease_until > now
        });
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
            if let Some(left_out) = self.listings.remove(&id) {
                self.left_out = left_out.into_iter().collect();
            }
            let Event::FeedResumed(cursor) = event else {
                return self.deliver(event);
            };
            self.resume_feed(cursor);
            self.deliver(event)?;
            if self.flaps_left > 0 {
                self.flaps_left -= 1;
                self.lose_feed();
            }
            return Ok(());
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
            88 => self.lose_claim(),
            89 => return self.jump_clock(),
            90..=91 => self.crash_agent(),
            92 if self.owns_board && self.rng.bool() => self.board_dies(),
            92 => self.board_outage(),
            93 => self.lose_feed(),
            94..=95 => self.unblock(),
            96 => self.cancel_task(),
            97 => self.relabel_role(),
            99 => return self.kill_or_follow(),
            98 if self.rng.u32(0..FLAP_EVERY) == 0 => {
                let restarts = u32::from(FEED_RESTARTS_MAX);
                self.flaps_left = self.rng.u32(restarts..3 * restarts);
                self.lose_feed();
            }
            98 => return self.restart(),
            _ => return self.advance(),
        }
        Ok(())
    }

    /// Every board call fails for one to nine minutes from now.
    fn board_outage(&mut self) {
        for run in self.core.runs() {
            self.down_in.insert(phase_name(run.phase));
        }
        self.down_until = Timestamp(self.now.0 + self.rng.u64(1..10) * MINUTE);
        self.trace
            .push(format!("board down until {:?}", self.down_until));
    }

    /// The board process the daemon started dies; it stays dead until a restart or the owner
    /// starts it.
    fn board_dies(&mut self) {
        for run in self.core.runs() {
            self.down_in.insert(phase_name(run.phase));
        }
        let minutes = self.rng.u64(1..=BOARD_DEAD_MINUTES_MAX);
        self.dead_until = Timestamp(self.now.0 + minutes * MINUTE);
        if self.rng.bool() {
            self.spawn_fails_until = self.dead_until;
        }
        self.trace
            .push(format!("board dead until {:?}", self.dead_until));
        let id = self.expect(None, "board");
        self.reply(id, Event::BoardExited);
    }

    /// Asks the core, as the owner, to kill a task's agent or to follow its log, sometimes for
    /// a task no agent runs, and checks the answer.
    fn kill_or_follow(&mut self) -> Result<(), String> {
        let running: Vec<TaskId> = (self.core.runs().iter())
            .filter(|run| run.phase == Phase::Running)
            .map(|run| run.task)
            .collect();
        let alive: Vec<TaskId> = (self.core.runs().iter())
            .filter(|run| run.agent_alive)
            .map(|run| run.task)
            .collect();
        let task = match running.is_empty() || self.rng.u32(0..4) == 0 {
            true => TaskId(self.rng.u64(1..2 * BOARD_TASKS_MAX as u64)),
            false => running[self.rng.usize(0..running.len())],
        };
        let kill = self.rng.bool();
        let request = match kill {
            true => Request::Kill(task),
            false => Request::Logs { task, follow: true },
        };
        let client = OWNER;
        self.replies.clear();
        self.deliver(Event::Client { client, request })?;
        let answer = (self.replies.iter())
            .find(|(id, _)| *id == client)
            .map(|(_, reply)| reply.clone());
        let expected = match (kill, answer.as_ref()) {
            (true, Some(Reply::Done)) => running.contains(&task),
            (true, Some(Reply::Refused { .. })) => !running.contains(&task),
            (false, Some(Reply::Log { follow, .. })) => *follow == alive.contains(&task),
            _ => false,
        };
        if !expected {
            return Err(format!("{request:?} for {task:?} was answered {answer:?}"));
        }
        self.deliver(Event::ClientGone(client))?;
        self.replies.clear();
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
        self.pass(pause)
    }

    /// The clock jumps past every lease between two ticks, whatever is in flight.
    fn jump_clock(&mut self) -> Result<(), String> {
        for run in self.core.runs() {
            self.jumped_in.insert(phase_name(run.phase));
        }
        let pause = self.config.lease_millis + self.rng.u64(SECOND..JUMP_PAST_LEASE_MAX);
        self.trace.push(format!("clock jumps {pause} ms"));
        self.pass(pause)
    }

    /// Time passes: leases lapse, agents due exit, and the core is told the time.
    fn pass(&mut self, pause: u64) -> Result<(), String> {
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
        self.leftovers.retain(|agent| agent.exit_at > now);
        for agent in done {
            self.reply(agent.spawn, Event::AgentExited(agent.run));
        }
        self.deliver(Event::Tick(now))
    }

    /// The daemon crashes and starts again: the core's state, its actions in flight and its
    /// agents are gone, and a new core rebuilds from the board, following the feed from the
    /// cursor the old one handled.
    fn restart(&mut self) -> Result<(), String> {
        for run in self.core.runs() {
            self.restarted_in.insert(phase_name(run.phase));
        }
        let saved = self.core.cursor();
        let crashed = std::mem::take(&mut self.agents);
        let unaccounted = self.recover(crashed)?;
        self.core = State::new(self.config.clone(), self.now);
        self.core.leave_to_lease(unaccounted);
        self.generation += 1;
        self.results.clear();
        self.outstanding.clear();
        self.log.clear();
        self.spawned.clear();
        self.refused.clear();
        self.listings.clear();
        self.replies.clear();
        self.told.clear();
        self.left_out.clear();
        self.listed_through = 0;
        self.flaps_left = 0;
        self.feed_restarts = 0;
        self.feed_since = self.now;
        self.feed_delivered = false;
        self.feed_failed = None;
        self.feed_retry_after = None;
        // The restarted daemon starts the board again if it is dead.
        self.dead_until = self.now;
        self.spawn_fails_until = self.now;
        self.board_restarts = 0;
        self.board_since = Some(self.now);
        self.stopped_now = false;
        self.feed = (self.history.iter())
            .filter(|event| event.cursor > saved)
            .cloned()
            .collect();
        self.feed_up = true;
        self.trace.push(format!("restart from {saved:?}"));
        self.deliver(Event::Tick(self.now))
    }

    /// The restarted daemon's recovery, from files that may be unreadable or lost and pids
    /// that may have been reused: every live agent of the crashed core must be stopped or have
    /// its task left to its lease, and no reused pid may be stopped.
    fn recover(&mut self, crashed: Vec<Agent>) -> Result<Unaccounted, String> {
        let (reused, live): (Vec<Agent>, Vec<Agent>) =
            crashed.into_iter().partition(|_| self.rng.u32(0..10) == 0);
        let record = |agent: &Agent| AgentRecord {
            task: agent.task,
            group: agent.group,
            start: agent.start,
        };
        let all: Vec<&Agent> = live.iter().chain(&reused).collect();
        let records: Vec<AgentRecord> = all.iter().map(|agent| record(agent)).collect();
        let mut pid_files = Vec::new();
        for agent in &all {
            pid_files.push(PidFile {
                task: agent.task,
                group: (self.rng.u32(0..8) != 0).then_some(agent.group),
                ack: (self.rng.u32(0..4) != 0).then_some(record(agent)),
            });
        }
        let records_read = (self.rng.u32(0..4) != 0).then_some(&records[..]);
        let pid_files_read = (self.rng.u32(0..6) != 0).then_some(&pid_files[..]);
        let started = |group: i32| {
            let alive = live.iter().find(|agent| agent.group == group);
            let stranger = reused.iter().find(|agent| agent.group == group);
            alive
                .map(|agent| agent.start)
                .or(stranger.map(|agent| agent.start + 1))
        };
        let recovery = recover(records_read, pid_files_read, &started);
        self.trace.push(format!("recovered {recovery:?}"));
        if let Some(record) =
            (recovery.stop.iter()).find(|r| reused.iter().any(|a| a.group == r.group))
        {
            return Err(format!("recovery stopped {record:?}, a reused pid"));
        }
        self.leftovers.clear();
        for agent in live {
            let stopped = recovery
                .stop
                .iter()
                .any(|record| record.group == agent.group);
            let left = match &recovery.unaccounted {
                Unaccounted::Every => true,
                Unaccounted::Tasks(tasks) => tasks.contains(&agent.task),
            };
            if left && !stopped {
                self.leftovers.push(agent);
            } else if !stopped {
                return Err(format!(
                    "recovery neither stopped nor accounted for {:?}'s agent, group {}",
                    agent.task, agent.group
                ));
            }
        }
        Ok(recovery.unaccounted)
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
        if id.0 > 1 && self.onto_rng.u32(0..3) == 0 {
            let onto = self.onto_rng.u64(1..id.0);
            labels.push(("onto".to_string(), format!("#{onto}")));
        }
        if self.rng.bool() && labels.len() == 1 {
            let branch = BRANCHES[self.rng.usize(0..BRANCHES.len())];
            labels.push(("branch".to_string(), branch.to_string()));
        }
        let queued = self.queue_rng.u32(0..QUEUED_EVERY) == 0;
        self.tasks.push(FakeTask {
            id,
            channel: if queued { QUEUE_CHANNEL } else { WORK_CHANNEL },
            state: TaskState::Ready,
            labels,
            epoch: 0,
            holder: None,
            generation: 0,
            profile: String::new(),
            lease_until: self.now,
            version: 0,
        });
        self.publish(self.tasks.len() - 1, EventKind::TaskCreated);
    }

    fn lose_feed(&mut self) {
        if self.feed_up {
            self.feed_up = false;
            self.feed.clear();
            let id = self.expect(None, "feed");
            self.reply(id, Event::FeedLost);
        }
    }

    /// A restarted feed replays from a little before the cursor it was asked for.
    fn resume_feed(&mut self, cursor: Cursor) {
        let from = cursor.0.saturating_sub(self.rng.u64(0..=REPLAY_BACK_MAX));
        self.feed = (self.history.iter())
            .filter(|event| event.cursor.0 > from)
            .cloned()
            .collect();
        self.feed_up = true;
    }

    /// The owner adds a role to an open task, or changes or removes the one it has.
    fn relabel_role(&mut self) {
        let open: Vec<usize> = (0..self.tasks.len())
            .filter(|index| !is_closed(self.tasks[*index].state))
            .collect();
        if open.is_empty() {
            return;
        }
        let index = open[self.rng.usize(0..open.len())];
        let role = ROLES[self.rng.usize(0..ROLES.len())].label;
        let labels = &mut self.tasks[index].labels;
        match labels.iter().position(|(name, _)| name == "role") {
            Some(found) if self.rng.bool() => drop(labels.remove(found)),
            Some(found) => labels[found].1 = role.to_string(),
            None => labels.push(("role".to_string(), role.to_string())),
        }
        self.publish(index, EventKind::LabelsChanged);
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
        self.tasks[index].version = self.cursor;
        let task = &self.tasks[index];
        let event = BoardEvent {
            cursor: Cursor(self.cursor),
            kind,
            task: task.id,
            channel: task.channel.to_string(),
            labels: task.labels.clone(),
            state: Some(task.state),
            actor: None,
        };
        if self.feed_up {
            self.feed.push_back(event.clone());
        }
        self.history.push(event);
    }

    fn deliver(&mut self, event: Event) -> Result<(), String> {
        self.note_result(&event);
        self.note_told(&event);
        self.note_feed(&event);
        if let Event::ClaimRefused(run) = event {
            let claiming = (self.core.runs().iter())
                .any(|entry| entry.id == run && entry.phase == Phase::Claiming);
            let task = self.log.get(&run).and_then(|log| log.task);
            if let Some(task) = task.filter(|_| claiming) {
                self.refused.insert(task.0);
            }
        }
        self.stopped_now = self.core.stopping() == Stopping::Now;
        let actions = step(&mut self.core, event.clone());
        self.trace.push(format!("{event:?} -> {actions:?}"));
        if matches!(event, Event::FeedLost)
            && !self.core.exited()
            && self.feed_restarts > FEED_RESTARTS_MAX
            && self.core.board() == BoardHealth::Up
        {
            return Err(format!(
                "the feed was lost {} times in a row, but the board still counts as up",
                self.feed_restarts
            ));
        }
        self.check_board(&event, &actions)?;
        self.check_feed(&event, &actions)?;
        self.check(&actions)?;
        for action in actions {
            self.perform(action)?;
        }
        Ok(())
    }

    /// Counts board restarts in a row as the core must, and checks it restarts the board it
    /// started no more than `BOARD_RESTARTS_MAX` times in a row and counts it down meanwhile.
    fn check_board(&mut self, event: &Event, actions: &[Action]) -> Result<(), String> {
        let restarts = (actions.iter())
            .filter(|action| matches!(action, Action::RestartBoard))
            .count();
        match event {
            Event::BoardRestarted => self.board_since = Some(self.now),
            Event::BoardExited if !self.core.exited() => {
                let since = self.board_since.take();
                let uptime = since.map_or(0, |since| self.now.0.saturating_sub(since.0));
                if uptime >= BOARD_UPTIME_MILLIS_MIN {
                    self.board_restarts = 0;
                }
                let expected = usize::from(self.board_restarts < BOARD_RESTARTS_MAX);
                if restarts != expected {
                    return Err(format!(
                        "the board exited after {} restarts in a row, and the core asked for \
                         {restarts} restarts",
                        self.board_restarts
                    ));
                }
                if self.core.board() == BoardHealth::Up {
                    return Err("the board exited, but it still counts as up".to_string());
                }
                self.board_given_up |= expected == 0;
                self.spawn_given_up |= expected == 0 && self.now < self.spawn_fails_until;
                self.board_restarts += u8::try_from(expected).expect("zero or one");
            }
            _ if restarts > 0 => {
                return Err(format!("{event:?} asked for a board restart"));
            }
            _ => {}
        }
        Ok(())
    }

    /// Follows the feed rule the core must: past `FEED_RESTARTS_MAX` losses in a row it stops
    /// restarting the feed and claims nothing until a feed tried `FEED_RETRY_MILLIS` apart
    /// delivers an event or stays up that long.
    fn check_feed(&mut self, event: &Event, actions: &[Action]) -> Result<(), String> {
        if self.core.exited() {
            return Ok(());
        }
        let now = self.now;
        let mut expected = 0;
        match (event, self.feed_retry_after) {
            (Event::FeedLost, _) if self.feed_failed.is_some() => {
                self.feed_retry_after = Some(now);
            }
            (Event::FeedLost, _) if self.feed_restarts > FEED_RESTARTS_MAX => {
                self.feed_failed = Some(now);
                self.feed_retry_after = Some(now);
                self.feed_given_up = true;
            }
            (Event::FeedLost, _) => expected = 1,
            (Event::Tick(_), None) if self.feed_failed.is_some() => {
                let uptime = now.0.saturating_sub(self.feed_since.0);
                if self.feed_delivered || uptime >= FEED_RETRY_MILLIS {
                    self.feed_failed = None;
                    self.feed_restarts = 0;
                    self.feed_recovered = true;
                }
            }
            (Event::Tick(_), Some(ended))
                if self.feed_failed.is_some()
                    && now.0.saturating_sub(ended.0) >= FEED_RETRY_MILLIS =>
            {
                self.feed_retry_after = None;
                self.feed_since = now;
                self.feed_delivered = false;
                expected = 1;
            }
            _ => {}
        }
        let restarts = (actions.iter())
            .filter(|action| matches!(action, Action::RestartFeed(_)))
            .count();
        if restarts != expected {
            return Err(format!(
                "{event:?} with feed restarts exhausted since {:?} asked for {restarts} feed \
                 restarts, not {expected}",
                self.feed_failed
            ));
        }
        let claims = (actions.iter()).any(|action| matches!(action, Action::Claim { .. }));
        if self.feed_failed.is_some() && (claims || self.core.board() == BoardHealth::Up) {
            return Err("the feed's restarts are exhausted, but the core claims".to_string());
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
            Event::ClaimLost(run) | Event::CloseLost(run) => (*run, "lost"),
            Event::BoardFailed(Call::Settle(run) | Call::Release(run)) => (*run, "failure"),
            _ => return,
        };
        let log = self.log.entry(run).or_default();
        match (entry, event) {
            ("claimed", _) => log.claimed = true,
            (_, Event::Prepared { branch, role, .. }) => {
                log.prepared = true;
                log.branch = branch.clone();
                log.launch_role = Some(role.clone());
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

    /// Keeps, per task, only what is newer than what the core was already told of it.
    fn note_told(&mut self, event: &Event) {
        if self.core.exited() {
            return;
        }
        match event {
            Event::Tick(_) => self.refused.clear(),
            Event::Feed(feed) => {
                // The core drops an event at or before its cursor, as one it already reflects.
                let through = self.listed_through.max(self.core.cursor().0);
                if let Some(state) = feed.state.filter(|_| feed.cursor.0 > through) {
                    let newer = (self.told.get(&feed.task.0))
                        .is_none_or(|told| told.cursor < feed.cursor.0);
                    if newer {
                        self.tell(feed.task, feed.cursor.0, state, &feed.labels);
                    }
                }
            }
            Event::Snapshot { tasks, cursor, .. } => {
                self.listed_through = self.listed_through.max(cursor.0);
                // A listing shows every open task it does not leave out, as of its cursor.
                let listed = |id: &u64| tasks.iter().any(|task| task.view.id.0 == *id);
                let left_out = &self.left_out;
                for (id, told) in self.told.iter_mut() {
                    if !listed(id) && !left_out.contains(id) && told.cursor <= cursor.0 {
                        told.cursor = cursor.0;
                        told.state = TaskState::Completed;
                    }
                }
                for ListedTask { view, .. } in tasks {
                    let version = view.version.0;
                    let newer =
                        (self.told.get(&view.id.0)).is_none_or(|told| told.cursor <= version);
                    if newer {
                        self.tell(view.id, version, view.state, &view.labels);
                    }
                }
            }
            _ => {}
        }
    }

    fn tell(&mut self, task: TaskId, cursor: u64, state: TaskState, labels: &[(String, String)]) {
        self.refused.remove(&task.0);
        self.left_out.remove(&task.0);
        let labels = labels.to_vec();
        self.told.insert(
            task.0,
            Told {
                cursor,
                state,
                labels,
            },
        );
    }

    /// Counts feed losses in a row: a feed that delivered an event or stayed up long enough
    /// starts the count again.
    fn note_feed(&mut self, event: &Event) {
        match event {
            Event::Feed(_) => self.feed_delivered = true,
            Event::FeedResumed(_) => {
                self.feed_since = self.now;
                self.feed_delivered = false;
            }
            Event::FeedLost if self.feed_failed.is_none() => {
                let uptime = self.now.0.saturating_sub(self.feed_since.0);
                if self.feed_delivered || uptime >= FEED_UPTIME_MILLIS_MIN {
                    self.feed_restarts = 0;
                }
                self.feed_restarts = self.feed_restarts.saturating_add(1);
            }
            _ => {}
        }
    }

    fn board_fails(&mut self) -> bool {
        self.now < self.down_until || self.now < self.dead_until || self.rng.u32(0..20) == 0
    }

    fn perform(&mut self, action: Action) -> Result<(), String> {
        match action {
            Action::Claim { run, task, profile } => {
                self.check_onto_free(task)?;
                self.claim(run, task, profile);
            }
            Action::Resume { run, task, profile } => self.resume(run, task, &profile),
            Action::Prepare { run, task } => {
                let id = self.expect(Some(run), "prepare");
                let branch = self.learn_branch(task);
                // Preparation reads the role the task has now, whatever it was claimed as.
                let role = (self.tasks.iter().find(|fake| fake.id == task))
                    .and_then(|fake| fake.labels.iter().find(|(name, _)| name == "role"))
                    .map(|(_, role)| role.clone());
                let event = match (self.rng.u32(0..10), role) {
                    (0, _) | (_, None) => Event::PrepareBlocked {
                        run,
                        kind: "invalid_brief".to_string(),
                        reason: "base is not a need".to_string(),
                    },
                    (_, Some(role)) => Event::Prepared { run, branch, role },
                };
                self.reply(id, event);
            }
            Action::Spawn { run, task, .. } => {
                let spawn = self.expect(Some(run), "spawn");
                self.spawned.insert(run);
                let exit_at = Timestamp(self.now.0 + self.rng.u64(MINUTE..70 * MINUTE));
                let stubborn = self.rng.u32(0..6) == 0;
                let group = self.next_group;
                self.next_group += 1;
                self.agents.push(Agent {
                    run,
                    task,
                    group,
                    start: self.now.0 / SECOND,
                    spawn,
                    exit_at,
                    stubborn,
                });
                self.check_launched_builders()?;
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
            Action::Release {
                run, task, epoch, ..
            } => self.close(run, task, epoch, None)?,
            Action::ListTasks => self.list(),
            Action::RestartBoard => {
                let id = self.expect(None, "board");
                let fails = self.now < self.spawn_fails_until;
                if fails || self.rng.u32(0..BOARD_RESTART_FAILS_EVERY) == 0 {
                    self.reply(id, Event::BoardExited);
                } else {
                    self.dead_until = self.now;
                    self.reply(id, Event::BoardRestarted);
                }
            }
            Action::RestartFeed(cursor) => {
                let id = self.expect(None, "feed");
                self.reply(id, Event::FeedResumed(cursor));
            }
            Action::Reply { client, reply } if client == OWNER => {
                self.replies.push((client, reply));
            }
            Action::Reply { .. } => {}
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
        let current = fake.generation == self.generation;
        (fake.holder == Some(run) && current && fake.lease_until > self.now).then_some(index)
    }

    /// Renews, as `profile`, a claim granted to a core before a restart.
    fn resume(&mut self, run: RunId, task: TaskId, profile: &str) {
        self.log.entry(run).or_default().task = Some(task);
        let id = self.expect(Some(run), "claim");
        if self.board_fails() {
            self.reply(id, Event::BoardFailed(Call::Claim(run)));
            return;
        }
        let now = self.now;
        let held = (self.tasks.iter()).position(|fake| {
            fake.id == task
                && fake.state == TaskState::Claimed
                && fake.holder.is_some()
                && fake.profile == profile
                && fake.lease_until > now
        });
        let Some(index) = held else {
            self.reply(id, Event::ClaimRefused(run));
            return;
        };
        let until = Timestamp(now.0 + self.config.lease_millis);
        let fake = &mut self.tasks[index];
        fake.holder = Some(run);
        fake.generation = self.generation;
        fake.lease_until = until;
        let epoch = fake.epoch;
        self.reply(id, Event::Claimed { run, epoch, until });
    }

    fn claim(&mut self, run: RunId, task: TaskId, profile: String) {
        self.log.entry(run).or_default().task = Some(task);
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
                fake.generation = self.generation;
                fake.profile = profile;
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
            self.reply(id, Event::CloseLost(run));
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
        let open: Vec<&FakeTask> = (self.tasks.iter())
            .filter(|fake| !is_closed(fake.state))
            .collect();
        let (listed, rest) = open.split_at(open.len().min(LISTING_TASKS_MAX));
        let left_out = rest.len();
        self.listings
            .insert(id, rest.iter().map(|fake| fake.id.0).collect());
        let tasks = (listed.iter())
            .map(|fake| ListedTask {
                view: TaskView {
                    id: fake.id,
                    channel: "work/sim".to_string(),
                    text: String::new(),
                    state: fake.state,
                    labels: fake.labels.clone(),
                    needs: Vec::new(),
                    epoch: fake.epoch,
                    claim: fake.holder.map(|_| Claim {
                        holder: format!("local:user:sim+{}", fake.profile),
                        until: fake.lease_until,
                    }),
                    version: Cursor(fake.version),
                },
                channel: fake.channel.to_string(),
            })
            .collect();
        // The cursor is read before the pages, so events may land between the two.
        let lag = self.rng.u64(0..=LISTING_LAG_MAX);
        let cursor = Cursor(self.cursor.saturating_sub(lag));
        self.reply(
            id,
            Event::Snapshot {
                tasks,
                cursor,
                left_out,
            },
        );
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
        let builders = (runs.iter())
            .filter(|run| self.config.roles[run.role].builds)
            .count();
        if builders > usize::from(self.config.builders_max) {
            let max = self.config.builders_max;
            return Err(format!(
                "invariant 1: {builders} runs of building roles, over {max}"
            ));
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
            if run.phase == Phase::Stopping(StopReason::Killed) {
                self.log.entry(run.id).or_default().killed = true;
            }
            let now = self.core.now();
            let covered = (self.outstanding.iter())
                .any(|entry| entry.1 == Some(run.id) && matches!(entry.2, "renew" | "stop"))
                || actions.iter().any(|action| {
                    matches!(action, Action::Renew { run: id, .. }
                        | Action::StopAgent { run: id, .. } if *id == run.id)
                });
            // Every phase but Claiming and Abandoning holds the claim and may write with it.
            let holds = !matches!(run.phase, Phase::Claiming | Phase::Abandoning);
            if holds && run.lease_until <= now && !covered {
                return Err(format!(
                    "invariant 3: {run:?} holds a lapsed lease at {now:?}, neither renewed \
                     nor abandoned"
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
        self.check_tasks_newest()?;
        let tracked = self.core.tasks().len();
        let untracked = self.core.untracked();
        let told_open = |id: &u64| self.told.get(id).is_some_and(|told| !is_closed(told.state));
        let unseen = self.left_out.iter().filter(|id| !told_open(id)).count();
        let open = (self.told.values())
            .filter(|told| !is_closed(told.state))
            .count()
            + unseen;
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
        let down = self.core.board() != BoardHealth::Up;
        let blocked = down || self.core.stopping() != Stopping::No;
        for action in actions {
            let (run, write) = match action {
                Action::Claim { run, task, profile } => {
                    self.log.entry(*run).or_default().profile = Some(profile.clone());
                    if blocked {
                        return Err(format!("invariant 9: {action:?} while down or stopping"));
                    }
                    let channel = self.tasks.iter().find(|fake| fake.id == *task);
                    let channel = channel.map(|fake| fake.channel);
                    if channel != Some(WORK_CHANNEL) {
                        return Err(format!("invariant 12: {action:?} claims in {channel:?}"));
                    }
                    if self.refused.contains(&task.0) {
                        return Err(format!(
                            "{action:?} retries a refused claim before a tick or news of its task"
                        ));
                    }
                    (*run, true)
                }
                Action::Resume { run, task, profile } => {
                    self.log.entry(*run).or_default().profile = Some(profile.clone());
                    if blocked {
                        return Err(format!("invariant 9: {action:?} while down or stopping"));
                    }
                    if let Some(agent) = self.leftovers.iter().find(|a| a.task == *task) {
                        return Err(format!(
                            "{action:?} resumes beside group {}, an agent from before",
                            agent.group
                        ));
                    }
                    (*run, true)
                }
                Action::Renew { run, .. } => (*run, true),
                Action::Release { run, profile, .. } => {
                    if down {
                        return Err(format!("{action:?} while the board is down"));
                    }
                    let claimed = self.log.entry(*run).or_default().profile.as_ref();
                    if claimed != Some(profile) {
                        return Err(format!("{action:?} is not as the claim's {claimed:?}"));
                    }
                    (*run, true)
                }
                Action::Settle {
                    run, settlement, ..
                } => {
                    if self.stopped_now {
                        return Err(format!("stop-now: {action:?} counts an attempt"));
                    }
                    if down {
                        return Err(format!("{action:?} while the board is down"));
                    }
                    let log = self.log.entry(*run).or_default();
                    log.settles += 1;
                    let judged = log.verdict.clone().map(Settlement::Verdict);
                    let based = match settlement {
                        _ if log.judged > 0 => {
                            log.judged == 1 && judged.as_ref() == Some(settlement)
                        }
                        Settlement::Block { .. } => log.blocked,
                        Settlement::Verdict(Verdict::Fail { reason }) => {
                            log.time_limited || (log.killed && reason == KILLED_REASON)
                        }
                        _ => false,
                    };
                    if !based || log.settles > 1 + log.settle_failures {
                        return Err(format!("invariant 6: {action:?} without one judgment"));
                    }
                    (*run, true)
                }
                Action::Spawn { run, .. } => {
                    let claimed = (self.core.runs().iter().find(|entry| entry.id == *run))
                        .map(|entry| self.config.roles[entry.role].label.clone());
                    let log = self.log.entry(*run).or_default();
                    if !log.claimed || !log.prepared {
                        return Err(format!("invariant 5: {action:?} before claim and prepare"));
                    }
                    if log.launch_role.is_none() || log.launch_role != claimed {
                        return Err(format!(
                            "invariant 11: {action:?} launches as {:?}, claimed as {claimed:?}",
                            log.launch_role
                        ));
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

    /// Live agents of building roles, by the role each launched with, are within the limit.
    fn check_launched_builders(&self) -> Result<(), String> {
        let builds = |run: &RunId| {
            let role = self.log.get(run).and_then(|log| log.launch_role.as_ref());
            (self.config.roles.iter()).any(|spec| Some(&spec.label) == role && spec.builds)
        };
        let builders = self
            .agents
            .iter()
            .filter(|agent| builds(&agent.run))
            .count();
        if builders > usize::from(self.config.builders_max) {
            let max = self.config.builders_max;
            return Err(format!(
                "invariant 1: {builders} agents launched as building roles, over {max}"
            ));
        }
        Ok(())
    }

    /// Every task the core tracks is as the newest version it was told, by cursor.
    fn check_tasks_newest(&self) -> Result<(), String> {
        if self.core.exited() {
            return Ok(());
        }
        for task in self.core.tasks() {
            let Some(told) = self.told.get(&task.id.0) else {
                return Err(format!(
                    "the core tracks {task:?}, which it was never told of"
                ));
            };
            let role = (told.labels.iter())
                .find(|(name, _)| name == "role")
                .and_then(|(_, label)| self.config.roles.iter().position(|r| r.label == *label));
            let branch = (told.labels.iter())
                .find(|(name, _)| name == "branch")
                .map(|(_, branch)| branch.clone());
            let newest = !is_closed(told.state)
                && told.state == task.state
                && role == task.role
                && branch == task.branch;
            if !newest {
                return Err(format!(
                    "stale state: the core holds {task:?}, but the newest it was told is {told:?}"
                ));
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

    /// Invariant 2 on onto: a task is not claimed while a settling run's worktree holds the
    /// branch of the task it works onto, so its preparation would find that branch busy.
    fn check_onto_free(&self, task: TaskId) -> Result<(), String> {
        let label_of = |task: TaskId, name: &str| {
            let fake = self.tasks.iter().find(|fake| fake.id == task)?;
            let found = fake.labels.iter().find(|(label, _)| label == name)?;
            Some(found.1.clone())
        };
        let onto = label_of(task, "onto").and_then(|onto| onto[1..].parse::<u64>().ok());
        let Some(branch) = onto.and_then(|onto| label_of(TaskId(onto), "branch")) else {
            return Ok(());
        };
        let busy = (self.core.runs().iter()).find(|run| {
            run.phase == Phase::Settling && run.branch.as_deref() == Some(branch.as_str())
        });
        match busy {
            Some(run) => Err(format!(
                "invariant 2: claimed {task:?} onto {branch} while {run:?} settles there"
            )),
            None => Ok(()),
        }
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

fn phase_name(phase: Phase) -> String {
    let phase = format!("{phase:?}");
    phase.split('(').next().unwrap_or_default().to_string()
}

fn is_closed(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled
    )
}
