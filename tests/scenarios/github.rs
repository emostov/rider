use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::fake_agent::Behaviour;
use crate::steps::*;
use crate::world::{World, write_executable};

const FAST_POLL: (&str, &str) = ("RIDER_TEST_MILLIS", "tick=20");
const RECORD_SECONDS_MAX: u64 = 30;
const QUIET_MILLIS: u64 = 3000;
const HEAD: &str = "2ae54426992ec8249006b5ea9b5531e8900062f3";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/github")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn given_gh_prints(world: &World, pr_list: &str, comments: &str) {
    fs::write(world.root.join("gh-pr.json"), fixture(pr_list)).expect("write the pr fixture");
    fs::write(world.root.join("gh-comments.json"), fixture(comments)).expect("write comments");
    let root = world.root.display();
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{root}/gh-calls.log'\ncase \"$1\" in\n  pr) cat \
         '{root}/gh-pr.json' ;;\n  api) cat '{root}/gh-comments.json' ;;\n  *) exit 1 ;;\nesac\n"
    );
    write_executable(&world.agents.join("gh"), &script);
}

fn gh_calls(world: &World) -> usize {
    fs::read_to_string(world.root.join("gh-calls.log")).map_or(0, |log| log.lines().count())
}

fn given_a_completed_task(world: &mut World) -> String {
    let task = when_the_lead_creates(world, &an_implement_task(world, "Add one work file."));
    given_the_agent_will(world, &task, &[Behaviour::Commit]);
    given_a_running_daemon_with(world, &[FAST_POLL]);
    assert_eq!(when_the_tasks_settle(world, &[&task]), vec!["completed"]);
    task
}

fn when_records_reach(world: &World, task: &str, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECONDS_MAX);
    loop {
        let records = task_records(world, task, "rider-github");
        if records.len() >= count || Instant::now() >= deadline {
            thread::sleep(Duration::from_millis(QUIET_MILLIS));
            return task_records(world, task, "rider-github");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn when_gh_is_called_again(world: &World) {
    when_gh_is_called_past(world, gh_calls(world));
}

fn when_gh_is_called_past(world: &World, before: usize) {
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECONDS_MAX);
    while gh_calls(world) <= before {
        assert!(Instant::now() < deadline, "{}", world.daemon_log());
        thread::sleep(Duration::from_millis(100));
    }
}

fn failed_record() -> Value {
    json!({"kind": "checks", "pr": 7, "head": HEAD, "state": "failed", "job": "test",
        "url": "https://github.com/example/repo/actions/runs/44868640008/job/136737934594"})
}

#[test]
fn ci_pending_then_failed_posts_one_record_and_a_restart_posts_it_not_again() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let task = given_a_completed_task(&mut world);
    when_gh_is_called_again(&world);
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        Vec::<Value>::new()
    );

    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    assert_eq!(when_records_reach(&world, &task, 1), vec![failed_record()]);

    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    given_a_running_daemon_with(&mut world, &[FAST_POLL]);
    when_gh_is_called_again(&world);
    when_gh_is_called_again(&world);
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        vec![failed_record()]
    );
}

#[test]
fn a_new_review_comment_and_a_pr_comment_post_one_record_each() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-review.json", "pulls-comments.json");
    let task = given_a_completed_task(&mut world);

    let records = when_records_reach(&world, &task, 3);

    let url = "https://github.com/example/repo/pull/7";
    assert_eq!(
        records,
        vec![
            json!({"kind": "review", "pr": 7, "head": HEAD, "state": "COMMENTED",
                "author": "reviewer", "excerpt": "Please split the parser out of the loop.",
                "url": url}),
            json!({"kind": "comment", "pr": 7, "head": HEAD, "author": "commenter",
                "excerpt": "Is the cap of 64 enough for a large swarm?",
                "url": format!("{url}#issuecomment-1")}),
            json!({"kind": "review", "pr": 7, "head": HEAD, "author": "reviewer",
                "file": "src/github.rs", "line": 30, "excerpt": "This loop needs a bound.",
                "url": format!("{url}#discussion_r1")}),
        ]
    );
}

#[test]
fn a_merged_pr_ends_the_watch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-merged.json", "pulls-comments-empty.json");
    let task = given_a_completed_task(&mut world);
    when_gh_is_called_past(&world, 0);
    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    let calls = gh_calls(&world);

    thread::sleep(Duration::from_millis(QUIET_MILLIS));

    assert_eq!(gh_calls(&world), calls);
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        Vec::<Value>::new()
    );
}
