//! Chosen orderings, bounds at their limit, and known gaps, each driven through the core by a
//! fixed sequence of events.

use rider::board::{Claim, ListedTask, TaskId, TaskState, TaskView, Timestamp};
use rider::core::{
    Action, CLIENTS_MAX, Call, ClientId, Config, ErrorCode, Event, LEASE_MILLIS,
    PREPARE_MILLIS_MAX, Phase, Reply, Request, RunId, SETTLE_PAUSE_MILLIS, SETTLE_TRIES_MAX,
    STOP_MILLIS_MAX, Settlement, State, StopReason, TASKS_MAX, UNTRACKED_MAX, Untracked, step,
};
use rider::daemon::EVENT_QUEUE_MAX;
use rider::feed::{BoardEvent, Cursor, EventKind};
use rider::roles::ROLES;
use rider::settle::Verdict;

const START: Timestamp = Timestamp(1_800_000_000_000);
const ROLE: &str = "implement";
const SECOND: u64 = 1000;
/// The profile the first slot of `ROLE` claims as, as the board names its holder.
const HOLDER: &str = "local:user:sim+";

/// One core and the board's cursor, driven by hand.
struct Script {
    state: State,
    now: Timestamp,
    cursor: u64,
}

impl Script {
    /// A core that has taken its first, empty, listing.
    fn new() -> Script {
        let mut script = Script::bare();
        let actions = script.step(Event::Tick(START));
        assert_eq!(actions, vec![Action::ListTasks]);
        script.snapshot(Vec::new());
        script
    }

    /// A core that has not yet been told anything.
    fn bare() -> Script {
        let config = Config::from_roles(&ROLES);
        Script {
            state: State::new(config, START),
            now: START,
            cursor: 0,
        }
    }

    fn step(&mut self, event: Event) -> Vec<Action> {
        step(&mut self.state, event)
    }

    fn snapshot(&mut self, tasks: Vec<ListedTask>) -> Vec<Action> {
        let cursor = Cursor(self.cursor);
        self.step(Event::Snapshot {
            tasks,
            cursor,
            left_out: 0,
        })
    }

    fn tick_after(&mut self, millis: u64) -> Vec<Action> {
        self.now = Timestamp(self.now.0 + millis);
        self.step(Event::Tick(self.now))
    }

    fn feed(
        &mut self,
        task: TaskId,
        kind: EventKind,
        labels: Vec<(String, String)>,
    ) -> Vec<Action> {
        self.cursor += 1;
        self.step(Event::Feed(BoardEvent {
            cursor: Cursor(self.cursor),
            kind,
            task,
            channel: "work/sim".to_string(),
            labels,
            state: Some(TaskState::Ready),
            actor: None,
        }))
    }

    /// A ready task of `ROLE` reaches the core, which claims it; returns the claiming run.
    fn ready(&mut self, task: TaskId) -> RunId {
        let actions = self.feed(task, EventKind::TaskCreated, role_labels());
        match actions.as_slice() {
            [
                Action::Claim {
                    run, task: claimed, ..
                },
            ] if *claimed == task => *run,
            other => panic!("a ready task was answered {other:?}, not one claim"),
        }
    }

    fn claimed(&mut self, run: RunId) -> Vec<Action> {
        let until = Timestamp(self.now.0 + LEASE_MILLIS);
        self.step(Event::Claimed {
            run,
            epoch: 1,
            until,
        })
    }

    /// A ready task is claimed, prepared and its agent spawned.
    fn running(&mut self, task: TaskId) -> RunId {
        let run = self.ready(task);
        assert_eq!(self.claimed(run), vec![Action::Prepare { run, task }]);
        let role = ROLE.to_string();
        let actions = self.step(Event::Prepared {
            run,
            branch: None,
            role,
        });
        assert!(
            matches!(actions.as_slice(), [Action::Spawn { run: spawned, .. }] if *spawned == run),
            "a prepared run was answered {actions:?}"
        );
        assert_eq!(self.phase(run), Some(Phase::Running));
        run
    }

    /// A running agent exits and is judged `Complete`; returns the settle the core sends.
    fn judged_complete(&mut self, run: RunId, task: TaskId) -> Vec<Action> {
        let actions = self.step(Event::AgentExited(run));
        assert_eq!(actions, vec![Action::Judge { run, task }]);
        self.step(Event::Judged {
            run,
            verdict: complete(),
            usage: None,
        })
    }

    fn phase(&self, run: RunId) -> Option<Phase> {
        let found = self.state.runs().iter().find(|entry| entry.id == run);
        found.map(|entry| entry.phase)
    }
}

fn role_labels() -> Vec<(String, String)> {
    vec![("role".to_string(), ROLE.to_string())]
}

fn complete() -> Verdict {
    Verdict::Complete {
        result: "done".to_string(),
    }
}

fn settle(run: RunId, task: TaskId) -> Action {
    Action::Settle {
        run,
        task,
        epoch: 1,
        settlement: Settlement::Verdict(complete()),
    }
}

/// A write that would count against, or close, the run's task.
fn writes(actions: &[Action], run: RunId) -> bool {
    actions.iter().any(|action| match action {
        Action::Settle { run: id, .. }
        | Action::Release { run: id, .. }
        | Action::Renew { run: id, .. }
        | Action::Judge { run: id, .. } => *id == run,
        _ => false,
    })
}

fn listed(
    id: TaskId,
    state: TaskState,
    labels: Vec<(String, String)>,
    holder: Option<String>,
) -> ListedTask {
    ListedTask {
        view: TaskView {
            id,
            channel: "work/sim".to_string(),
            text: String::new(),
            state,
            labels,
            needs: Vec::new(),
            epoch: 1,
            claim: holder.map(|holder| Claim {
                holder,
                until: Timestamp(START.0 + LEASE_MILLIS),
            }),
            version: Cursor(1),
        },
    }
}

#[test]
fn a_claim_lost_as_the_agent_exits_writes_nothing() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.running(task);
    let lost = script.step(Event::ClaimLost(run));
    assert_eq!(lost, vec![Action::StopAgent { run, task }]);
    let exited = script.step(Event::AgentExited(run));
    assert!(!writes(&exited, run), "after a lost claim: {exited:?}");
    assert_eq!(script.phase(run), None);
}

#[test]
fn a_claim_lost_as_the_exited_agent_is_judged_writes_nothing() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.running(task);
    let exited = script.step(Event::AgentExited(run));
    assert_eq!(exited, vec![Action::Judge { run, task }]);
    let lost = script.step(Event::ClaimLost(run));
    assert!(!writes(&lost, run), "a claim lost while judging: {lost:?}");
    assert_eq!(script.phase(run), None);
    let verdict = complete();
    let judged = script.step(Event::Judged {
        run,
        verdict,
        usage: None,
    });
    assert!(
        !writes(&judged, run),
        "a judgment after a lost claim: {judged:?}"
    );
}

#[test]
fn stop_now_during_settling_releases_on_retry() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.running(task);
    assert_eq!(script.judged_complete(run, task), vec![settle(run, task)]);
    let client = ClientId(1);
    let down = script.step(Event::Client {
        client,
        request: Request::Down { now: true },
    });
    assert_eq!(
        down,
        vec![Action::Reply {
            client,
            reply: Reply::Accepted
        }]
    );
    assert_eq!(script.phase(run), Some(Phase::Settling));
    let failed = script.step(Event::BoardFailed(Call::Settle(run)));
    assert!(
        !writes(&failed, run),
        "a failed settle retried at once: {failed:?}"
    );
    let claimed = Some(format!("{HOLDER}{}", profile(&script, run)));
    script.snapshot(vec![listed(
        task,
        TaskState::Claimed,
        role_labels(),
        claimed,
    )]);
    let retried = script.tick_after(SETTLE_PAUSE_MILLIS + SECOND);
    let release = (retried.iter()).find(|action| matches!(action, Action::Release { .. }));
    let settles = (retried.iter()).any(|action| matches!(action, Action::Settle { .. }));
    assert!(
        release.is_some() && !settles,
        "after stop-now the retry was {retried:?}"
    );
    let settled = script.step(Event::Settled(run));
    assert!(
        settled.contains(&Action::Exit(0)),
        "the last run settled, but the core sent {settled:?}"
    );
}

fn profile(script: &Script, run: RunId) -> String {
    let found = script.state.runs().iter().find(|entry| entry.id == run);
    let entry = found.unwrap_or_else(|| panic!("{run:?} is not running"));
    let role = &script.state.config().roles[entry.role];
    format!("{}-{}-{}", role.label, entry.slot, role.model_slug)
}

#[test]
fn a_kill_during_claiming_is_refused_and_the_claim_goes_on() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.ready(task);
    let client = ClientId(1);
    let killed = script.step(Event::Client {
        client,
        request: Request::Kill(task),
    });
    let refused = matches!(
        killed.as_slice(),
        [Action::Reply {
            reply: Reply::Refused {
                code: ErrorCode::NotRunning,
                ..
            },
            ..
        }]
    );
    assert!(refused, "a kill while claiming was answered {killed:?}");
    assert_eq!(script.phase(run), Some(Phase::Claiming));
    assert_eq!(script.claimed(run), vec![Action::Prepare { run, task }]);
}

#[test]
fn a_feed_lost_between_ready_and_claimed_restarts_once_and_claims_on() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.ready(task);
    let lost = script.step(Event::FeedLost);
    assert_eq!(
        lost,
        vec![Action::RestartFeed(Cursor(1)), Action::ListTasks]
    );
    assert_eq!(script.claimed(run), vec![Action::Prepare { run, task }]);
    let resumed = script.step(Event::FeedResumed(Cursor(1)));
    assert!(
        !writes(&resumed, run),
        "a resumed feed was answered {resumed:?}"
    );
    assert_eq!(script.phase(run), Some(Phase::Preparing));
}

#[test]
fn a_clock_jump_past_the_lease_abandons_every_run() {
    let mut script = Script::new();
    let running = script.running(TaskId(1));
    let jumped = script.tick_after(LEASE_MILLIS + 60 * SECOND);
    assert!(
        jumped.contains(&Action::StopAgent {
            run: running,
            task: TaskId(1)
        }),
        "a run past its lease was answered {jumped:?}"
    );
    assert!(
        !writes(&jumped, running),
        "a lapsed run still writes: {jumped:?}"
    );
    assert_eq!(script.phase(running), Some(Phase::Abandoning));
    let exited = script.step(Event::AgentExited(running));
    assert!(
        !writes(&exited, running),
        "after a lapsed lease: {exited:?}"
    );
    assert_eq!(script.phase(running), None);
}

#[test]
fn tasks_past_tasks_max_are_counted_untracked() {
    let mut script = Script::new();
    let total = TASKS_MAX + UNTRACKED_MAX;
    for id in 1..=total {
        let actions = script.feed(TaskId(id as u64), EventKind::TaskCreated, Vec::new());
        assert_eq!(actions, Vec::new());
    }
    assert_eq!(script.state.tasks().len(), TASKS_MAX);
    assert_eq!(script.state.untracked(), Untracked::Exactly(UNTRACKED_MAX));
    let past = TaskId(total as u64 + 1);
    script.feed(past, EventKind::TaskCreated, Vec::new());
    assert_eq!(script.state.tasks().len(), TASKS_MAX);
    assert_eq!(script.state.untracked(), Untracked::AtLeast(UNTRACKED_MAX));
}

#[test]
fn a_burst_past_event_queue_max_is_handled_in_order() {
    let task = TaskId(1);
    let mut script = Script::new();
    script.feed(task, EventKind::TaskCreated, Vec::new());
    let mut last = String::new();
    for index in 0..=EVENT_QUEUE_MAX {
        last = format!("b{index}");
        let labels = vec![("branch".to_string(), last.clone())];
        assert_eq!(
            script.feed(task, EventKind::LabelsChanged, labels),
            Vec::new()
        );
    }
    assert_eq!(script.state.cursor(), Cursor(script.cursor));
    let tracked = script.state.tasks().iter().find(|entry| entry.id == task);
    assert_eq!(tracked.and_then(|entry| entry.branch.clone()), Some(last));
}

#[test]
fn clients_past_clients_max_are_answered_but_not_followed() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.running(task);
    for id in 0..=CLIENTS_MAX as u64 {
        let client = ClientId(id + 10);
        let actions = script.step(Event::Client {
            client,
            request: Request::Logs { task, follow: true },
        });
        let follow = (id as usize) < CLIENTS_MAX;
        let reply = Reply::Log { task, follow };
        assert_eq!(actions, vec![Action::Reply { client, reply }]);
    }
    for id in 0..=CLIENTS_MAX as u64 {
        let client = ClientId(id + 100);
        let actions = script.step(Event::Client {
            client,
            request: Request::Down { now: false },
        });
        assert_eq!(
            actions,
            vec![Action::Reply {
                client,
                reply: Reply::Accepted
            }]
        );
    }
    assert_eq!(script.phase(run), Some(Phase::Running));
    let killed = script.step(Event::Client {
        client: ClientId(1),
        request: Request::Kill(task),
    });
    assert_eq!(script.phase(run), Some(Phase::Stopping(StopReason::Killed)));
    assert!(killed.contains(&Action::StopAgent { run, task }));
    let exited = script.step(Event::AgentExited(run));
    let ended = (exited.iter())
        .filter(|action| {
            matches!(
                action,
                Action::Reply {
                    reply: Reply::End,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        ended, CLIENTS_MAX,
        "the followers told the log ended: {exited:?}"
    );
}

#[test]
#[ignore = "known gap: settle tries run out during a board outage and the verdict is lost"]
fn an_agent_finishing_during_an_outage_is_settled_when_the_board_returns() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.running(task);
    assert_eq!(script.judged_complete(run, task), vec![settle(run, task)]);
    for _ in 0..u32::from(SETTLE_TRIES_MAX) + 2 {
        script.step(Event::BoardFailed(Call::Settle(run)));
        script.tick_after(SETTLE_PAUSE_MILLIS + SECOND);
    }
    let claimed = Some(format!("{HOLDER}{}", profile(&script, run)));
    let back = script.snapshot(vec![listed(
        task,
        TaskState::Claimed,
        role_labels(),
        claimed,
    )]);
    let retried = script.tick_after(SETTLE_PAUSE_MILLIS + SECOND);
    let sent = back
        .iter()
        .chain(&retried)
        .any(|action| *action == settle(run, task));
    assert!(
        sent,
        "the board returned, and the core sent {back:?} then {retried:?}"
    );
}

#[test]
fn a_failed_startup_renewal_is_retried_when_the_board_returns() {
    let task = TaskId(1);
    let mut script = Script::bare();
    assert_eq!(script.step(Event::Tick(START)), vec![Action::ListTasks]);
    let config = Config::from_roles(&ROLES);
    let role = &config.roles[0];
    let holder = Some(format!("{HOLDER}{}-1-{}", role.label, role.model_slug));
    let labels = vec![("role".to_string(), role.label.clone())];
    let task_listed = || listed(task, TaskState::Claimed, labels.clone(), holder.clone());
    let first = script.snapshot(vec![task_listed()]);
    let run = match first.as_slice() {
        [Action::Resume { run, .. }] => *run,
        other => panic!("a held claim at start-up was answered {other:?}"),
    };
    script.step(Event::BoardFailed(Call::Claim(run)));
    script.tick_after(60 * SECOND);
    let back = script.snapshot(vec![task_listed()]);
    let again = match back.as_slice() {
        [Action::Resume { run, task: t, .. }, Action::ListTasks] if *t == task => *run,
        other => panic!("the board returned, and the core sent {other:?}"),
    };
    assert_eq!(
        script.claimed(again),
        vec![Action::Prepare { run: again, task }]
    );
    let later = script.snapshot(vec![task_listed()]);
    assert_eq!(later, vec![]);
}

#[test]
fn a_startup_renewal_failing_after_a_retry_snapshot_is_retried_again() {
    let (first, second) = (TaskId(1), TaskId(2));
    let mut script = Script::bare();
    assert_eq!(script.step(Event::Tick(START)), vec![Action::ListTasks]);
    let config = Config::from_roles(&ROLES);
    let role = &config.roles[0];
    let labels = vec![("role".to_string(), role.label.clone())];
    let held = |task: TaskId, slot: u8| {
        let holder = format!("{HOLDER}{}-{slot}-{}", role.label, role.model_slug);
        listed(task, TaskState::Claimed, labels.clone(), Some(holder))
    };
    let both = || vec![held(first, 1), held(second, 2)];
    let resumes = |actions: &[Action]| -> Vec<(TaskId, RunId)> {
        let resumed = actions.iter().filter_map(|action| match action {
            Action::Resume { run, task, .. } => Some((*task, *run)),
            _ => None,
        });
        resumed.collect()
    };
    let start = resumes(&script.snapshot(both()));
    let [(_, run_a), (_, run_b)] = start.as_slice() else {
        panic!("two held claims at start-up were answered {start:?}");
    };
    let (run_a, run_b) = (*run_a, *run_b);
    script.step(Event::BoardFailed(Call::Claim(run_a)));
    script.tick_after(10 * SECOND);
    let retried = resumes(&script.snapshot(both()));
    let [(task, retry_a)] = retried.as_slice() else {
        panic!("the board returned, and the core resumed {retried:?}");
    };
    assert_eq!(*task, first);
    let retry_a = *retry_a;
    script.step(Event::BoardFailed(Call::Claim(run_b)));
    script.claimed(retry_a);
    script.tick_after(10 * SECOND);
    let last = resumes(&script.snapshot(both()));
    assert_eq!(
        last.iter().map(|(task, _)| *task).collect::<Vec<_>>(),
        vec![second]
    );
}

#[test]
fn a_prepare_retry_due_at_the_phase_deadline_is_not_sent() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.ready(task);
    assert_eq!(script.claimed(run), vec![Action::Prepare { run, task }]);
    script.tick_after(PREPARE_MILLIS_MAX - SETTLE_PAUSE_MILLIS);
    script.step(Event::BoardFailed(Call::Prepare(run)));
    script.snapshot(vec![listed(
        task,
        TaskState::Claimed,
        role_labels(),
        Some(format!("{HOLDER}{}", profile(&script, run))),
    )]);
    let due = script.tick_after(SETTLE_PAUSE_MILLIS);
    let prepares = due
        .iter()
        .filter(|action| matches!(action, Action::Prepare { .. }));
    assert_eq!(prepares.count(), 0, "at the deadline the core sent {due:?}");
}

/// A run of `built` on a branch has sent its closing write when its renewal finds the claim
/// lost, and a fix `onto` `target` is then ready; returns the run and what the fix was answered.
fn settling_after_a_lost_renewal(
    script: &mut Script,
    built: TaskId,
    fix: TaskId,
    target: TaskId,
) -> (RunId, Vec<Action>) {
    let run = script.ready(built);
    assert_eq!(
        script.claimed(run),
        vec![Action::Prepare { run, task: built }]
    );
    let branch = Some(format!("rider/{}-work", built.0));
    let role = ROLE.to_string();
    let spawned = script.step(Event::Prepared { run, branch, role });
    assert!(matches!(spawned.as_slice(), [Action::Spawn { .. }]));
    assert_eq!(script.judged_complete(run, built), vec![settle(run, built)]);
    script.cursor += 1;
    script.step(Event::Feed(BoardEvent {
        cursor: Cursor(script.cursor),
        kind: EventKind::TaskCompleted,
        task: built,
        channel: "work/sim".to_string(),
        labels: role_labels(),
        state: Some(TaskState::Completed),
        actor: None,
    }));
    assert_eq!(script.step(Event::ClaimLost(run)), Vec::new());
    let mut labels = role_labels();
    labels.push(("onto".to_string(), target.0.to_string()));
    let waiting = script.feed(fix, EventKind::TaskCreated, labels);
    assert_eq!(script.phase(run), Some(Phase::Abandoning));
    (run, waiting)
}

#[test]
fn a_claim_lost_while_settling_keeps_the_branch_until_the_worktree_is_released() {
    let (built, fix) = (TaskId(1), TaskId(2));
    let mut script = Script::new();
    let (run, waiting) = settling_after_a_lost_renewal(&mut script, built, fix, built);

    let settled = script.step(Event::Settled(run));

    assert_eq!(waiting, Vec::new());
    assert!(
        matches!(settled.as_slice(), [Action::Claim { task, .. }] if *task == fix),
        "the fix was answered {settled:?} once the worktree was released"
    );
}

#[test]
fn a_fix_onto_an_earlier_task_waits_while_a_run_that_lost_its_claim_is_still_closing() {
    let (earlier, built, fix) = (TaskId(1), TaskId(2), TaskId(3));
    let mut script = Script::new();
    let (run, waiting) = settling_after_a_lost_renewal(&mut script, built, fix, earlier);

    let settled = script.step(Event::Settled(run));

    assert_eq!(waiting, Vec::new());
    assert!(
        matches!(settled.as_slice(), [Action::Claim { task, .. }] if *task == fix),
        "the fix was answered {settled:?} once the worktree was released"
    );
}

#[test]
fn a_closing_write_that_fails_after_a_lost_renewal_ends_the_run() {
    let mut script = Script::new();
    let (run, _) = settling_after_a_lost_renewal(&mut script, TaskId(1), TaskId(2), TaskId(9));

    script.step(Event::BoardFailed(Call::Settle(run)));

    assert_eq!(script.phase(run), None);
}

#[test]
fn a_closing_write_that_never_answers_after_a_lost_renewal_ends_the_run_at_the_stop_limit() {
    let mut script = Script::new();
    let (run, _) = settling_after_a_lost_renewal(&mut script, TaskId(1), TaskId(2), TaskId(9));

    script.tick_after(STOP_MILLIS_MAX - 1);
    let before = script.phase(run);
    script.tick_after(1);

    assert_eq!(before, Some(Phase::Abandoning));
    assert_eq!(script.phase(run), None);
}

#[test]
fn a_closing_write_that_finds_the_claim_lost_ends_the_run_at_once() {
    let task = TaskId(1);
    let mut script = Script::new();
    let run = script.running(task);
    assert_eq!(script.judged_complete(run, task), vec![settle(run, task)]);

    let lost = script.step(Event::CloseLost(run));

    assert!(!writes(&lost, run), "the lost run was answered {lost:?}");
    assert_eq!(script.phase(run), None);
}
