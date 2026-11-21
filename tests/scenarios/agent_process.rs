use std::fs;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::World;

const KILL_GRACE_SECONDS: u64 = 10;
const LOG_BYTES_MAX: u64 = 64 * 1024 * 1024;
const _: () = assert!(fake_agent::FLOOD_BYTES > LOG_BYTES_MAX);

fn then_it_completed_with_its_summary(world: &World, task: &str) {
    let read: Value = serde_json::from_str(&task_text(world, task)).expect("vb read JSON");
    let summary = fake_agent::summary(task, 0);
    assert_eq!(
        (task_state(world, task), &read["message"]["task"]["result"]),
        ("completed".to_string(), &summary.into())
    );
}

#[test]
fn an_agent_that_ignores_sigterm_at_its_time_limit_is_killed_after_the_grace() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::HoldIgnoringTerm]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "attempt=1000,tick=200")]);
    let run = when_the_agent_has_started(&world, &task);

    let started = Instant::now();
    let states = when_the_tasks_settle(&world, &[&task]);
    let waited = started.elapsed();
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (vec!["failed".to_string()], Some(0))
    );
    assert!(!is_alive(run.pid), "the agent {} still runs", run.pid);
    let grace = Duration::from_secs(KILL_GRACE_SECONDS);
    assert!(
        waited >= grace && waited < grace * 3,
        "failed after {waited:?}"
    );
    let text = task_text(&world, &task);
    assert!(
        text.contains("the attempt reached its time limit"),
        "{text}"
    );
}

#[test]
fn a_process_an_agent_leaves_in_its_group_is_killed_when_the_agent_exits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::CommitLeavingOrphan]);

    let started = Instant::now();
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let pid = fake_agent::leftover(&world.root, &task);
    assert!(
        when_the_process_ends_within(pid, 2),
        "the leftover {pid} still runs"
    );
    let waited = started.elapsed();
    let leftover = Duration::from_secs(fake_agent::LEFTOVER_SECONDS);
    assert!(waited < leftover, "settled after {waited:?}");
    then_it_completed_with_its_summary(&world, &task);
}

#[test]
fn a_process_an_agent_moved_out_of_its_group_does_not_stall_the_attempt() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::CommitLeavingEscapee]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    let pid = fake_agent::leftover(&world.root, &task);
    let escaped_still_runs = is_alive(pid);
    let _ = std::process::Command::new("/bin/kill")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .status();
    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    assert!(
        escaped_still_runs,
        "the escaped process {pid} ended before the task settled"
    );
    then_it_completed_with_its_summary(&world, &task);
}

#[test]
fn an_agents_log_stops_at_its_bound_while_the_agent_runs_on_to_its_exit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::CommitFlooding]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let log = world.home.join("logs").join(&task).join("1.jsonl");
    let bytes = fs::metadata(&log).expect("the attempt's log").len();
    assert_eq!(bytes, LOG_BYTES_MAX);
    then_it_completed_with_its_summary(&world, &task);
}
