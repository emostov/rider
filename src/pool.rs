//! Worker slots: each repeatedly claims the next ready task for its role and runs it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::board::{Board, BoardError, TaskId, one_line};
use crate::core::{LEASE_MILLIS, ROLES_MAX};
use crate::roles::{AGENTS_MAX, RoleSpec, display_name, profile_name};
use crate::run_task::{Ran, RunError};
use crate::settle::Verdict;

/// The wait of every claim: none, because a claiming slot holds a gate place, and a place
/// held while waiting is one a role with ready work cannot use.
pub const CLAIM_WITHOUT_WAITING: u32 = 0;
/// The pause, outside the gate, after a claim that found nothing ready.
pub const IDLE_PAUSE_SECONDS: u64 = 3;
/// The length of one call to `Effects::pause`; a slot checks `stop` between calls.
pub const PAUSE_STEP_MILLIS: u64 = 1000;
/// Consecutive failed claims after which a slot stops: five minutes of board outage.
pub const CLAIM_RETRIES_MAX: u32 = 60;
/// A run of failed claims is reported on its first failure and on every this-many after.
pub const CLAIM_ERRORS_REPORTED_EVERY: u32 = 10;
/// The pause after a failed claim before the next one.
pub const CLAIM_RETRY_PAUSE_SECONDS: u64 = 5;
/// How long a slot waits for a gate place before it checks `stop` again.
pub const GATE_WAIT_MILLIS: u64 = 1000;
/// Upper bound on the waits of one try for a gate place: two hours.
pub const GATE_WAITS_MAX: u32 = 2 * 3600;
// `rest` counts its pause steps as seconds.
const _: () = assert!(PAUSE_STEP_MILLIS == 1000);
const SECONDS_PER_DAY: u64 = 24 * 3600;
/// The pause after a failed release before the next try.
pub const RELEASE_RETRY_PAUSE_SECONDS: u64 = 5;
/// Tries to release a task, one every `RELEASE_RETRY_PAUSE_SECONDS`, until its lease has lapsed.
pub const RELEASE_TRIES_MAX: u64 = LEASE_MILLIS / 1000 / RELEASE_RETRY_PAUSE_SECONDS;
/// A run of failed releases is reported on its first failure and on every this-many after.
pub const RELEASE_ERRORS_REPORTED_EVERY: u64 = 60;
const _: () = assert!(RELEASE_TRIES_MAX > 1 && RELEASE_ERRORS_REPORTED_EVERY > 1);
/// Upper bound on the claims one slot makes before it stops.
pub const SLOT_ROUNDS_MAX: u32 = 1_000_000;
/// Upper bound on the slots of every role together.
pub const SLOTS_MAX: usize = ROLES_MAX * AGENTS_MAX as usize;
/// Upper bound, in characters, on the result, reason, or error shown in one event line.
pub const DETAIL_CHARS_MAX: usize = 120;

/// One worker: a role and its 1-based slot number, with its board profile and its name.
#[derive(Debug, Clone)]
pub struct Slot {
    pub role: &'static RoleSpec,
    pub number: u8,
    pub profile: String,
    pub name: String,
}

/// One slot per `RoleSpec::pool_slots` of every role in `table`, in table order.
pub fn slots(table: &'static [RoleSpec]) -> Vec<Slot> {
    assert!(!table.is_empty() && table.len() <= ROLES_MAX);
    let mut all = Vec::with_capacity(SLOTS_MAX);
    for role in table {
        for number in 1..=role.pool_slots {
            all.push(Slot {
                role,
                number,
                profile: profile_name(role, number),
                name: display_name(role, number),
            });
        }
    }
    assert!(all.len() <= SLOTS_MAX && all.len() >= table.len());
    all
}

/// A counting gate that lets at most its limit of holders through at once, and at most its
/// builders limit of holders that build.
pub struct Gate {
    limit: u8,
    builders_limit: u8,
    running: Mutex<Running>,
    freed: Condvar,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Running {
    all: u8,
    builders: u8,
}

/// A place in the gate, given back when dropped, a panic's unwinding included.
pub struct GatePass<'a> {
    gate: &'a Gate,
    builds: bool,
}

impl Gate {
    /// A gate that lets at most `limit` holders, of which at most `builders_limit` build,
    /// through at once.
    pub fn new(limit: u8, builders_limit: u8) -> Gate {
        assert!(limit > 0 && limit <= AGENTS_MAX);
        assert!(builders_limit > 0 && builders_limit <= AGENTS_MAX);
        Gate {
            limit,
            builders_limit,
            running: Mutex::new(Running::default()),
            freed: Condvar::new(),
        }
    }

    /// Waits for a free place, and a free builder place when `builds`, and takes it; or returns
    /// `None` once `stop` is set or the wait passes `GATE_WAITS_MAX` waits.
    pub fn enter_unless(&self, stop: &AtomicBool, builds: bool) -> Option<GatePass<'_>> {
        // The counts are changed only by `enter_unless` and `drop`, which cannot panic while
        // holding the lock except on a broken invariant, so a poisoned lock still holds true
        // counts.
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        for _ in 0..GATE_WAITS_MAX {
            if stop.load(Ordering::SeqCst) {
                return None;
            }
            if running.all < self.limit && (!builds || running.builders < self.builders_limit) {
                running.all += 1;
                running.builders += u8::from(builds);
                assert!(running.all <= self.limit && running.builders <= self.builders_limit);
                return Some(GatePass { gate: self, builds });
            }
            let wait = Duration::from_millis(GATE_WAIT_MILLIS);
            let waited = self.freed.wait_timeout(running, wait);
            running = waited.unwrap_or_else(PoisonError::into_inner).0;
        }
        assert!(running.all <= self.limit);
        None
    }
}

impl Drop for GatePass<'_> {
    fn drop(&mut self) {
        let mut running = self
            .gate
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(running.all > 0 && running.all <= self.gate.limit);
        assert!(!self.builds || running.builders > 0);
        running.all -= 1;
        running.builders -= u8::from(self.builds);
        drop(running);
        // A waiter that builds and one that does not may both wait; wake every one to recheck.
        self.gate.freed.notify_all();
    }
}

/// Something a slot reports, one line each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    Claimed,
    Completed(&'a str),
    Failed(&'a str),
    Blocked,
    /// The task was handed back unrun, for the reason given.
    Released(&'a str),
    Error(&'a str),
}

/// The line reporting `event` of the slot or profile `name`, about task `id` when there is one,
/// at `now` shown as UTC `HH:MM:SSZ`.
pub fn event_line(event: Event, id: Option<TaskId>, name: &str, now: SystemTime) -> String {
    assert!(!name.is_empty() && !name.contains('\n'));
    let time = match now.duration_since(UNIX_EPOCH) {
        Ok(since) => {
            let day = since.as_secs() % SECONDS_PER_DAY;
            format!("{:02}:{:02}:{:02}Z", day / 3600, day % 3600 / 60, day % 60)
        }
        Err(_) => "--:--:--Z".to_string(),
    };
    assert_eq!(time.len(), "HH:MM:SSZ".len());
    let (word, detail) = match event {
        Event::Claimed => ("claimed", None),
        Event::Completed(result) => ("completed", Some(result)),
        Event::Failed(reason) => ("failed", Some(reason)),
        Event::Blocked => ("blocked", None),
        Event::Released(reason) => ("released", Some(reason)),
        Event::Error(message) => ("error", Some(message)),
    };
    let mut line = match id {
        Some(id) => format!("{time} {word} #{} {name}", id.0),
        None => format!("{time} {word} {name}"),
    };
    if let Some(detail) = detail {
        let short: String = one_line(detail).chars().take(DETAIL_CHARS_MAX).collect();
        line.push_str(": ");
        line.push_str(short.trim_end());
    }
    assert!(!line.contains('\n'));
    line
}

/// Why a slot's loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotEnd {
    Stopped,
    ClaimsFailing,
    RoundsExhausted,
}

/// What a slot does besides reaching the board: run a claimed task, pause for one step of
/// `PAUSE_STEP_MILLIS`, and report one line.
pub struct Effects<'a> {
    pub run: &'a dyn Fn(TaskId) -> Result<Ran, RunError>,
    pub pause: &'a dyn Fn(),
    pub say: &'a dyn Fn(&str),
    pub now: &'a dyn Fn() -> SystemTime,
}

/// Claims and runs tasks for `slot` from `channel` until `stop` is set or claims keep failing.
pub fn slot_loop(
    slot: &Slot,
    channel: &str,
    board: &dyn Board,
    gate: &Gate,
    stop: &AtomicBool,
    effects: &Effects,
) -> SlotEnd {
    assert!(channel.starts_with("work/"));
    assert_eq!(slot.profile, profile_name(slot.role, slot.number));
    let say = |event: Event, id: Option<TaskId>| {
        (effects.say)(&event_line(event, id, &slot.name, (effects.now)()))
    };
    let mut failures: u32 = 0;
    for _ in 0..SLOT_ROUNDS_MAX {
        // The place is taken before the claim, so a claimed task starts at once and its lease
        // cannot run out while it waits for a place.
        let Some(pass) = gate.enter_unless(stop, slot.role.builds) else {
            if stop.load(Ordering::SeqCst) {
                return SlotEnd::Stopped;
            }
            continue;
        };
        let id = match board.claim_ready(channel, slot.role.label, CLAIM_WITHOUT_WAITING) {
            Ok(Some(id)) => id,
            Ok(None) => {
                drop(pass);
                failures = 0;
                rest(IDLE_PAUSE_SECONDS, stop, effects.pause);
                continue;
            }
            Err(error) => {
                drop(pass);
                failures += 1;
                assert!(failures <= CLAIM_RETRIES_MAX);
                if failures == 1 {
                    say(Event::Error(&error.to_string()), None);
                } else if failures % CLAIM_ERRORS_REPORTED_EVERY == 1 {
                    let message = format!("{error} ({failures} claims in a row failed)");
                    say(Event::Error(&message), None);
                }
                if failures == CLAIM_RETRIES_MAX {
                    let message =
                        format!("{CLAIM_RETRIES_MAX} claims in a row failed; this slot stops");
                    say(Event::Error(&message), None);
                    return SlotEnd::ClaimsFailing;
                }
                rest(CLAIM_RETRY_PAUSE_SECONDS, stop, effects.pause);
                continue;
            }
        };
        failures = 0;
        say(Event::Claimed, Some(id));
        let ran = (effects.run)(id);
        drop(pass);
        match ran {
            Ok(Ran::Settled(Verdict::Complete { result })) => {
                say(Event::Completed(&result), Some(id))
            }
            Ok(Ran::Settled(Verdict::Fail { reason })) => say(Event::Failed(&reason), Some(id)),
            Ok(Ran::Settled(Verdict::LeaveBlocked)) => say(Event::Blocked, Some(id)),
            Err(RunError::Board(error)) => say(Event::Error(&error.to_string()), Some(id)),
            Err(RunError::LostClaim(state)) => {
                let message = format!("the task is no longer claimed; it is {state:?}");
                say(Event::Error(&message), Some(id));
            }
            // It goes back to be claimed by a slot of its new role, under that role's gate.
            Ok(Ran::RoleChanged { claimed, found }) => {
                let reason = format!("claimed as {claimed}, it is now {found}");
                release(board, id, &reason, &say, effects.pause);
            }
        }
    }
    let message = format!("{SLOT_ROUNDS_MAX} claims made; this slot stops");
    say(Event::Error(&message), None);
    SlotEnd::RoundsExhausted
}

/// Releases claimed task `id` for `reason`, retrying a failed release, stopping or not, until
/// the board takes it or the claim's lease has lapsed.
fn release(
    board: &dyn Board,
    id: TaskId,
    reason: &str,
    say: &dyn Fn(Event, Option<TaskId>),
    pause: &dyn Fn(),
) {
    assert!(!reason.is_empty());
    for tries in 1..=RELEASE_TRIES_MAX {
        match board.release(id) {
            Ok(()) => {
                say(Event::Released(reason), Some(id));
                return;
            }
            Err(error @ BoardError::ClaimLost { .. }) => {
                let message = format!("{error} (the claim is gone; not retried)");
                say(Event::Error(&message), Some(id));
                return;
            }
            Err(error) if tries % RELEASE_ERRORS_REPORTED_EVERY == 1 => {
                let message = format!("{error} (release try {tries}; it is retried)");
                say(Event::Error(&message), Some(id));
            }
            Err(_) => {}
        }
        for _ in 0..RELEASE_RETRY_PAUSE_SECONDS {
            pause();
        }
    }
    let message = format!("{RELEASE_TRIES_MAX} releases failed; the claim lapses with its lease");
    say(Event::Error(&message), Some(id));
}

/// Pauses for `seconds` in steps, returning early once `stop` is set.
fn rest(seconds: u64, stop: &AtomicBool, pause: &dyn Fn()) {
    assert!(seconds > 0 && seconds <= CLAIM_RETRY_PAUSE_SECONDS.max(IDLE_PAUSE_SECONDS));
    for _ in 0..seconds {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        pause();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::board::testing::FakeBoard;
    use crate::board::{BoardError, TaskState, TaskView};
    use crate::roles::ROLES;

    fn slot(label: &str, number: u8) -> Slot {
        let found = slots(&ROLES)
            .into_iter()
            .find(|slot| slot.role.label == label && slot.number == number);
        found.unwrap_or_else(|| panic!("no slot {label} {number}"))
    }

    fn quiet(_: &str) {}

    fn no_pause() {}

    fn epoch() -> SystemTime {
        UNIX_EPOCH
    }

    #[derive(Debug, Clone, Copy)]
    enum Step {
        Fail,
        Empty,
        Task(u64),
    }

    /// A board, shareable between slot threads, whose claims follow `steps` for `role` (any
    /// role when `None`) and otherwise find nothing ready.
    struct QueueBoard {
        steps: Mutex<VecDeque<Step>>,
        role: Option<&'static str>,
        /// The wait of every claim.
        waits: Mutex<Vec<u32>>,
        /// The tasks running now, kept by the test's `run`.
        running: AtomicUsize,
        /// `running` at each claim.
        running_at_claims: Mutex<Vec<usize>>,
    }

    impl QueueBoard {
        fn new(steps: Vec<Step>) -> QueueBoard {
            QueueBoard {
                steps: Mutex::new(steps.into()),
                role: None,
                waits: Mutex::new(Vec::new()),
                running: AtomicUsize::new(0),
                running_at_claims: Mutex::new(Vec::new()),
            }
        }
    }

    impl Board for QueueBoard {
        fn read_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
            panic!("the pool never reads task {}", id.0)
        }

        fn claim_ready(
            &self,
            _: &str,
            role_label: &str,
            wait_seconds: u32,
        ) -> Result<Option<TaskId>, BoardError> {
            let running = self.running.load(Ordering::SeqCst);
            self.running_at_claims.lock().unwrap().push(running);
            self.waits.lock().unwrap().push(wait_seconds);
            if self.role.is_some_and(|role| role != role_label) {
                // A claim that waits holds its slot's place for the wait, scaled down here.
                if wait_seconds > 0 {
                    thread::sleep(Duration::from_millis(100));
                }
                return Ok(None);
            }
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(Step::Fail) => Err(BoardError::Io("claim failed".to_string())),
                Some(Step::Empty) => Ok(None),
                Some(Step::Task(id)) => Ok(Some(TaskId(id))),
                None => {
                    thread::sleep(Duration::from_millis(1));
                    Ok(None)
                }
            }
        }

        fn set_label(&self, _: TaskId, _: &str, _: &str) -> Result<(), BoardError> {
            panic!("the pool never labels a task")
        }

        fn reply(&self, _: TaskId, _: &str) -> Result<(), BoardError> {
            panic!("the pool never replies")
        }

        fn post_structured(&self, _: TaskId, _: &str, _: &str) -> Result<(), BoardError> {
            panic!("the pool never posts")
        }

        fn complete(&self, _: TaskId, _: &str) -> Result<(), BoardError> {
            panic!("the pool never completes a task itself")
        }

        fn fail(&self, _: TaskId, _: &str) -> Result<(), BoardError> {
            panic!("the pool never fails a task itself")
        }

        fn block(&self, _: TaskId, _: &str, _: &str) -> Result<(), BoardError> {
            panic!("the pool never blocks a task itself")
        }

        fn claim_task(&self, id: TaskId) -> Result<TaskView, BoardError> {
            panic!("the pool never claims task {} by id", id.0)
        }

        fn renew(&self, id: TaskId) -> Result<TaskView, BoardError> {
            panic!("the pool never renews task {}", id.0)
        }

        fn release(&self, id: TaskId) -> Result<(), BoardError> {
            panic!("the pool never releases task {}", id.0)
        }

        fn list_open(&self, subtree: &str) -> Result<crate::board::Listing, BoardError> {
            panic!("the pool never lists {subtree}")
        }
    }

    #[test]
    fn slots_follow_the_role_table() {
        let all = slots(&ROLES);
        let expected: usize = ROLES.iter().map(|role| usize::from(role.pool_slots)).sum();
        assert_eq!(all.len(), expected);
        assert!(all.iter().any(|s| s.profile == "implement-2-opus-5-5"));
        assert!(all.iter().any(|s| s.profile == "security-1-mythos-5-2"));
        let review = slot("review", 2);
        assert_eq!(review.profile, "review-2-sol-6-1");
        assert_eq!(review.name, "Review 2 (Sol 6.1)");
        assert_eq!(review.role.label, "review");
    }

    #[test]
    fn never_more_than_four_tasks_run_at_once() {
        const TASKS: u64 = 12;
        let board = QueueBoard::new((1..=TASKS).map(Step::Task).collect());
        let gate = Gate::new(AGENTS_MAX, AGENTS_MAX);
        let stop = AtomicBool::new(false);
        let running = &board.running;
        let running_max = AtomicUsize::new(0);
        let ran = Mutex::new(Vec::new());
        let run = |id: TaskId| -> Result<Ran, RunError> {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            running_max.fetch_max(now, Ordering::SeqCst);
            // Wait, within a bound, for a second task to start, so the test shows real overlap.
            for _ in 0..200 {
                if running_max.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            thread::sleep(Duration::from_millis(20));
            running.fetch_sub(1, Ordering::SeqCst);
            let mut ran = ran.lock().unwrap();
            ran.push(id.0);
            if ran.len() == usize::try_from(TASKS).unwrap() {
                stop.store(true, Ordering::SeqCst);
            }
            Ok(Ran::Settled(Verdict::LeaveBlocked))
        };
        let all = slots(&ROLES);
        let ends: Vec<SlotEnd> = thread::scope(|scope| {
            let handles: Vec<_> = all
                .iter()
                .map(|slot| {
                    let (board, gate, stop, run) = (&board, &gate, &stop, &run);
                    scope.spawn(move || {
                        let effects = Effects {
                            run,
                            pause: &no_pause,
                            say: &quiet,
                            now: &epoch,
                        };
                        slot_loop(slot, "work/demo", board, gate, stop, &effects)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(ends, vec![SlotEnd::Stopped; all.len()]);
        let max = running_max.load(Ordering::SeqCst);
        assert!(max <= usize::from(AGENTS_MAX), "{max} tasks ran at once");
        assert!(max >= 2, "tasks never overlapped");
        let at_claims = board.running_at_claims.into_inner().unwrap();
        assert!(at_claims.len() >= usize::try_from(TASKS).unwrap());
        let busiest = at_claims.iter().max().copied().unwrap();
        assert!(
            busiest < usize::from(AGENTS_MAX),
            "claimed with {busiest} tasks running"
        );
        let mut ran = ran.into_inner().unwrap();
        ran.sort_unstable();
        assert_eq!(ran, (1..=TASKS).collect::<Vec<u64>>());
    }

    #[test]
    fn a_slot_claims_only_its_own_role() {
        let board = FakeBoard::new(Vec::new());
        board.claims.borrow_mut().push_back(TaskId(7));
        let stop = AtomicBool::new(false);
        let ran = Mutex::new(Vec::new());
        let run = |id: TaskId| -> Result<Ran, RunError> {
            ran.lock().unwrap().push(id.0);
            stop.store(true, Ordering::SeqCst);
            Ok(Ran::Settled(Verdict::Complete {
                result: "done".to_string(),
            }))
        };
        let lines = Mutex::new(Vec::new());
        let say = |line: &str| lines.lock().unwrap().push(line.to_string());
        let effects = Effects {
            run: &run,
            pause: &no_pause,
            say: &say,
            now: &epoch,
        };
        let review = slot("review", 1);
        let end = slot_loop(
            &review,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        assert_eq!(
            *board.calls.borrow(),
            vec!["claim_ready work/demo review 0"]
        );
        assert_eq!(ran.into_inner().unwrap(), vec![7]);
        assert_eq!(
            lines.into_inner().unwrap(),
            vec![
                "00:00:00Z claimed #7 Review 1 (Sol 6.1)",
                "00:00:00Z completed #7 Review 1 (Sol 6.1): done",
            ]
        );
    }

    #[test]
    fn stop_ends_every_loop() {
        let stop = AtomicBool::new(true);
        let run = |id: TaskId| -> Result<Ran, RunError> { panic!("ran task {}", id.0) };
        let effects = Effects {
            run: &run,
            pause: &no_pause,
            say: &quiet,
            now: &epoch,
        };
        let gate = Gate::new(AGENTS_MAX, AGENTS_MAX);
        for slot in slots(&ROLES) {
            let board = FakeBoard::new(Vec::new());
            board.claims.borrow_mut().push_back(TaskId(1));
            let end = slot_loop(&slot, "work/demo", &board, &gate, &stop, &effects);
            assert_eq!(end, SlotEnd::Stopped);
            assert_eq!(*board.calls.borrow(), Vec::<String>::new());
        }
    }

    #[test]
    fn a_stop_during_a_task_lets_it_finish_and_claims_nothing_more() {
        let board = FakeBoard::new(Vec::new());
        board.claims.borrow_mut().extend([TaskId(3), TaskId(4)]);
        let stop = AtomicBool::new(false);
        let ran = Mutex::new(Vec::new());
        let run = |id: TaskId| -> Result<Ran, RunError> {
            stop.store(true, Ordering::SeqCst);
            ran.lock().unwrap().push(id.0);
            Ok(Ran::Settled(Verdict::Fail {
                reason: "tests failed".to_string(),
            }))
        };
        let lines = Mutex::new(Vec::new());
        let say = |line: &str| lines.lock().unwrap().push(line.to_string());
        let effects = Effects {
            run: &run,
            pause: &no_pause,
            say: &say,
            now: &epoch,
        };
        let implement = slot("implement", 2);
        let end = slot_loop(
            &implement,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        assert_eq!(
            *board.calls.borrow(),
            vec!["claim_ready work/demo implement 0"]
        );
        assert_eq!(ran.into_inner().unwrap(), vec![3]);
        assert_eq!(
            lines.into_inner().unwrap()[1],
            "00:00:00Z failed #3 Implement 2 (Opus 5.5): tests failed"
        );
    }

    #[test]
    fn board_error_on_claim_is_reported_and_retried_after_a_pause() {
        let board = FakeBoard::new(Vec::new());
        board.failing.borrow_mut().push("claim_ready");
        board.claims.borrow_mut().push_back(TaskId(9));
        let stop = AtomicBool::new(false);
        let pauses = AtomicUsize::new(0);
        let pause = || {
            let steps = usize::try_from(CLAIM_RETRY_PAUSE_SECONDS).unwrap();
            if pauses.fetch_add(1, Ordering::SeqCst) + 1 == 2 * steps {
                board.failing.borrow_mut().clear();
            }
        };
        let run = |id: TaskId| -> Result<Ran, RunError> {
            assert_eq!(id, TaskId(9));
            stop.store(true, Ordering::SeqCst);
            Ok(Ran::Settled(Verdict::LeaveBlocked))
        };
        let lines = Mutex::new(Vec::new());
        let say = |line: &str| lines.lock().unwrap().push(line.to_string());
        let effects = Effects {
            run: &run,
            pause: &pause,
            say: &say,
            now: &epoch,
        };
        let polish = slot("polish", 1);
        let end = slot_loop(
            &polish,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        let steps = usize::try_from(CLAIM_RETRY_PAUSE_SECONDS).unwrap();
        assert_eq!(pauses.load(Ordering::SeqCst), 2 * steps);
        assert_eq!(board.calls.borrow().len(), 3);
        assert_eq!(
            lines.into_inner().unwrap(),
            vec![
                "00:00:00Z error Polish 1 (Sonnet 5.5): vb exited with code 1: claim_ready failed",
                "00:00:00Z claimed #9 Polish 1 (Sonnet 5.5)",
                "00:00:00Z blocked #9 Polish 1 (Sonnet 5.5)",
            ]
        );
    }

    #[test]
    fn a_slot_stops_after_too_many_failed_claims_in_a_row() {
        let board = FakeBoard::new(Vec::new());
        board.failing.borrow_mut().push("claim_ready");
        let stop = AtomicBool::new(false);
        let pauses = AtomicUsize::new(0);
        let pause = || {
            pauses.fetch_add(1, Ordering::SeqCst);
        };
        let run = |id: TaskId| -> Result<Ran, RunError> { panic!("ran task {}", id.0) };
        let lines = Mutex::new(Vec::new());
        let say = |line: &str| lines.lock().unwrap().push(line.to_string());
        let effects = Effects {
            run: &run,
            pause: &pause,
            say: &say,
            now: &epoch,
        };
        let security = slot("security", 1);
        let end = slot_loop(
            &security,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::ClaimsFailing);
        let retries = usize::try_from(CLAIM_RETRIES_MAX).unwrap();
        assert_eq!(board.calls.borrow().len(), retries);
        let steps = usize::try_from(CLAIM_RETRY_PAUSE_SECONDS).unwrap();
        assert_eq!(pauses.load(Ordering::SeqCst), (retries - 1) * steps);
        let name = "00:00:00Z error Security 1 (Mythos 5.2)";
        let error = "vb exited with code 1: claim_ready failed";
        let mut expected = vec![format!("{name}: {error}")];
        for failures in [11, 21, 31, 41, 51] {
            expected.push(format!(
                "{name}: {error} ({failures} claims in a row failed)"
            ));
        }
        expected.push(format!(
            "{name}: 60 claims in a row failed; this slot stops"
        ));
        assert_eq!(lines.into_inner().unwrap(), expected);
    }

    #[test]
    fn a_run_error_is_reported_and_the_slot_carries_on() {
        let board = FakeBoard::new(Vec::new());
        board.claims.borrow_mut().extend([TaskId(5), TaskId(6)]);
        let stop = AtomicBool::new(false);
        let run = |id: TaskId| -> Result<Ran, RunError> {
            if id == TaskId(5) {
                return Err(RunError::LostClaim(TaskState::Ready));
            }
            stop.store(true, Ordering::SeqCst);
            Err(RunError::Board(BoardError::Io("pipe closed".to_string())))
        };
        let lines = Mutex::new(Vec::new());
        let say = |line: &str| lines.lock().unwrap().push(line.to_string());
        let effects = Effects {
            run: &run,
            pause: &no_pause,
            say: &say,
            now: &epoch,
        };
        let review = slot("review", 2);
        let end = slot_loop(
            &review,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        assert_eq!(
            lines.into_inner().unwrap(),
            vec![
                "00:00:00Z claimed #5 Review 2 (Sol 6.1)",
                "00:00:00Z error #5 Review 2 (Sol 6.1): the task is no longer claimed; it is Ready",
                "00:00:00Z claimed #6 Review 2 (Sol 6.1)",
                concat!(
                    "00:00:00Z error #6 Review 2 (Sol 6.1): ",
                    "reading from or writing to vb failed: pipe closed"
                ),
            ]
        );
    }

    #[test]
    fn a_task_relabeled_after_its_claim_is_released_unrun() {
        let board = FakeBoard::new(Vec::new());
        board.claims.borrow_mut().push_back(TaskId(5));
        let stop = AtomicBool::new(false);
        let run = |_: TaskId| -> Result<Ran, RunError> {
            stop.store(true, Ordering::SeqCst);
            Ok(Ran::RoleChanged {
                claimed: "review",
                found: "implement",
            })
        };
        let lines = Mutex::new(Vec::new());
        let say = |line: &str| lines.lock().unwrap().push(line.to_string());
        let effects = Effects {
            run: &run,
            pause: &no_pause,
            say: &say,
            now: &epoch,
        };
        let end = slot_loop(
            &slot("review", 2),
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        let calls = ["claim_ready work/demo review 0", "release 5"];
        assert_eq!(*board.calls.borrow(), calls.map(str::to_string));
        assert_eq!(
            lines.into_inner().unwrap(),
            vec![
                "00:00:00Z claimed #5 Review 2 (Sol 6.1)",
                concat!(
                    "00:00:00Z released #5 Review 2 (Sol 6.1): ",
                    "claimed as review, it is now implement"
                ),
            ]
        );
    }

    #[test]
    fn nothing_ready_resets_the_failure_count() {
        let mut steps = vec![Step::Fail; 4];
        steps.push(Step::Empty);
        steps.extend([Step::Fail; 4]);
        steps.push(Step::Task(8));
        let board = QueueBoard::new(steps);
        let stop = AtomicBool::new(false);
        let pauses = AtomicUsize::new(0);
        let pause = || {
            pauses.fetch_add(1, Ordering::SeqCst);
        };
        let ran = Mutex::new(Vec::new());
        let run = |id: TaskId| -> Result<Ran, RunError> {
            ran.lock().unwrap().push(id.0);
            stop.store(true, Ordering::SeqCst);
            Ok(Ran::Settled(Verdict::LeaveBlocked))
        };
        let effects = Effects {
            run: &run,
            pause: &pause,
            say: &quiet,
            now: &epoch,
        };
        let implement = slot("implement", 1);
        let end = slot_loop(
            &implement,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        let retry = usize::try_from(CLAIM_RETRY_PAUSE_SECONDS).unwrap();
        let idle = usize::try_from(IDLE_PAUSE_SECONDS).unwrap();
        assert_eq!(pauses.load(Ordering::SeqCst), 8 * retry + idle);
        assert_eq!(ran.into_inner().unwrap(), vec![8]);
    }

    #[test]
    fn the_gate_admits_at_most_its_capacity_and_frees_a_place_on_panic() {
        let gate = Gate::new(AGENTS_MAX, AGENTS_MAX);
        let panicked = thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _pass = gate.enter_unless(&AtomicBool::new(false), true);
                    panic!("an agent run panicked");
                })
                .join()
        });
        assert!(panicked.is_err());
        assert_eq!(*gate.running.lock().unwrap(), Running::default());
        let go = AtomicBool::new(false);
        let passes: Vec<Option<GatePass>> = (0..AGENTS_MAX)
            .map(|_| gate.enter_unless(&go, false))
            .collect();
        assert!(passes.iter().all(Option::is_some));
        let full = Running {
            all: AGENTS_MAX,
            builders: 0,
        };
        assert_eq!(*gate.running.lock().unwrap(), full);
        drop(passes);
        assert_eq!(*gate.running.lock().unwrap(), Running::default());
    }

    #[test]
    fn a_full_builders_limit_holds_back_builders_but_not_others() {
        let gate = Gate::new(4, 2);
        let go = AtomicBool::new(false);
        let builders: Vec<Option<GatePass>> =
            (0..2).map(|_| gate.enter_unless(&go, true)).collect();
        assert!(builders.iter().all(Option::is_some));
        let stop = AtomicBool::new(false);
        let third = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(100));
                stop.store(true, Ordering::SeqCst);
            });
            gate.enter_unless(&stop, true).is_some()
        });
        assert!(!third, "a third builder entered a gate that allows two");
        let other = gate.enter_unless(&go, false);
        assert!(other.is_some());
        assert_eq!(
            *gate.running.lock().unwrap(),
            Running {
                all: 3,
                builders: 2
            }
        );
        drop(builders);
        let builder = gate.enter_unless(&go, true);
        assert!(builder.is_some());
        assert_eq!(
            *gate.running.lock().unwrap(),
            Running {
                all: 2,
                builders: 1
            }
        );
    }

    #[test]
    fn a_role_with_ready_work_gets_a_place_from_idle_slots() {
        let mut board = QueueBoard::new(vec![Step::Task(1)]);
        board.role = Some("review");
        let gate = Gate::new(4, 4);
        let stop = AtomicBool::new(false);
        let ran = AtomicBool::new(false);
        let run = |id: TaskId| -> Result<Ran, RunError> {
            assert_eq!(id, TaskId(1));
            ran.store(true, Ordering::SeqCst);
            stop.store(true, Ordering::SeqCst);
            Ok(Ran::Settled(Verdict::LeaveBlocked))
        };
        let pause = || thread::sleep(Duration::from_millis(5));
        let idle = ["implement", "light"].map(|label| [slot(label, 1), slot(label, 2)]);
        let mut contenders: Vec<Slot> = idle.into_iter().flatten().collect();
        contenders.push(slot("review", 1));
        assert_eq!(contenders.len(), usize::from(gate.limit) + 1);
        let started = Instant::now();
        thread::scope(|scope| {
            for contender in &contenders {
                let (board, gate, stop, run, pause) = (&board, &gate, &stop, &run, &pause);
                scope.spawn(move || {
                    let effects = Effects {
                        run,
                        pause,
                        say: &quiet,
                        now: &epoch,
                    };
                    slot_loop(contender, "work/demo", board, gate, stop, &effects)
                });
            }
            // Ends a starved run, so a failure shows as an assertion rather than a hang.
            for _ in 0..200 {
                if ran.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::SeqCst);
        });
        assert!(
            ran.load(Ordering::SeqCst),
            "the review slot never got a place"
        );
        let waited = started.elapsed();
        assert!(waited < Duration::from_secs(1), "{waited:?}");
        let waits = board.waits.into_inner().unwrap();
        assert!(waits.iter().all(|wait| *wait == 0), "{waits:?}");
    }

    #[test]
    fn stop_cuts_an_idle_pause_short() {
        let board = FakeBoard::new(Vec::new());
        let stop = AtomicBool::new(false);
        let pauses = AtomicUsize::new(0);
        let pause = || {
            pauses.fetch_add(1, Ordering::SeqCst);
            stop.store(true, Ordering::SeqCst);
        };
        let run = |id: TaskId| -> Result<Ran, RunError> { panic!("ran task {}", id.0) };
        let effects = Effects {
            run: &run,
            pause: &pause,
            say: &quiet,
            now: &epoch,
        };
        let polish = slot("polish", 1);
        let end = slot_loop(
            &polish,
            "work/demo",
            &board,
            &Gate::new(AGENTS_MAX, AGENTS_MAX),
            &stop,
            &effects,
        );
        assert_eq!(end, SlotEnd::Stopped);
        const { assert!(IDLE_PAUSE_SECONDS > 1) };
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
        assert_eq!(
            *board.calls.borrow(),
            vec!["claim_ready work/demo polish 0"]
        );
    }

    #[test]
    fn a_stopped_gate_gives_no_place() {
        let gate = Gate::new(AGENTS_MAX, AGENTS_MAX);
        assert!(gate.enter_unless(&AtomicBool::new(true), false).is_none());
        assert_eq!(*gate.running.lock().unwrap(), Running::default());
    }

    #[test]
    fn a_slot_waiting_for_a_place_returns_soon_after_stop() {
        let gate = Gate::new(AGENTS_MAX, AGENTS_MAX);
        let go = AtomicBool::new(false);
        let full: Vec<Option<GatePass>> = (0..AGENTS_MAX)
            .map(|_| gate.enter_unless(&go, false))
            .collect();
        assert!(full.iter().all(Option::is_some));
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let (end, calls) = thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let board = FakeBoard::new(Vec::new());
                board.claims.borrow_mut().push_back(TaskId(1));
                let run = |id: TaskId| -> Result<Ran, RunError> { panic!("ran task {}", id.0) };
                let effects = Effects {
                    run: &run,
                    pause: &no_pause,
                    say: &quiet,
                    now: &epoch,
                };
                let review = slot("review", 1);
                let end = slot_loop(&review, "work/demo", &board, &gate, &stop, &effects);
                (end, board.calls.into_inner())
            });
            thread::sleep(Duration::from_millis(50));
            stop.store(true, Ordering::SeqCst);
            waiting.join().unwrap()
        });
        assert_eq!(end, SlotEnd::Stopped);
        assert_eq!(
            calls,
            Vec::<String>::new(),
            "a slot without a place claims nothing"
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(GATE_WAIT_MILLIS * 3),
            "{waited:?}"
        );
        drop(full);
    }

    #[test]
    fn event_lines_name_the_task_and_slot_on_one_short_line() {
        let name = "Implement 1 (Opus 5.5)";
        let id = Some(TaskId(42));
        let at = UNIX_EPOCH + Duration::from_secs(3 * SECONDS_PER_DAY + 13 * 3600 + 4 * 60 + 5);
        assert_eq!(
            event_line(Event::Claimed, id, name, at),
            "13:04:05Z claimed #42 Implement 1 (Opus 5.5)"
        );
        assert_eq!(
            event_line(Event::Blocked, id, name, at),
            "13:04:05Z blocked #42 Implement 1 (Opus 5.5)"
        );
        assert_eq!(
            event_line(Event::Completed("Added\n  the store.\n"), id, name, at),
            "13:04:05Z completed #42 Implement 1 (Opus 5.5): Added the store."
        );
        let long = "x".repeat(300);
        assert_eq!(
            event_line(Event::Failed(&long), id, name, at),
            format!(
                "13:04:05Z failed #42 Implement 1 (Opus 5.5): {}",
                "x".repeat(DETAIL_CHARS_MAX)
            )
        );
        assert_eq!(
            event_line(Event::Error("vb is gone"), None, name, at),
            "13:04:05Z error Implement 1 (Opus 5.5): vb is gone"
        );
        assert_eq!(
            event_line(
                Event::Completed("ünïcode ".repeat(40).as_str()),
                id,
                "p",
                at
            )
            .chars()
            .count(),
            "13:04:05Z completed #42 p: ".len() + DETAIL_CHARS_MAX - 1
        );
        let before_1970 = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(
            event_line(Event::Claimed, id, name, before_1970),
            "--:--:--Z claimed #42 Implement 1 (Opus 5.5)"
        );
    }
}
