//! Black-box scenarios: the real `rider pool` against a real board, a scratch repository, and
//! fake agents, each written as Given / When / Then.

#[expect(
    dead_code,
    reason = "behaviours and readers for the scenarios still to come"
)]
mod fake_agent;
#[expect(dead_code, reason = "steps for the scenarios still to come")]
mod steps;
mod world;

use std::fs;

use serde_json::json;

use fake_agent::Behaviour;
use steps::*;
use world::{MAIN, PROJECT};

#[test]
fn an_implement_task_completes_with_its_commit_and_a_result_record() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));
    given_the_agent_will(&world, &task, &[Behaviour::Commit]);

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&task]);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(exit, Some(0));
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    then_the_worktree_is_removed(&world, &task);
    then_only_the_repository_is_a_worktree(&world);
    let record = json!({
        "branch": branch,
        "head": branch_head(&world, &branch),
        "commits": 1,
        "summary": fake_agent::summary(&task, 0),
    });
    assert_eq!(task_records(&world, &task, "rider-result"), vec![record]);
}

#[test]
fn a_stacked_task_is_cut_from_its_base_tasks_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let base = when_the_lead_creates(&world, &an_implement_task(&world, "Add the base work."));
    let stacked = NewTask::new("Add the stacked work.")
        .label("repo", world.repo_label())
        .label("base", &base)
        .needs(&base)
        .role("implement");
    let stacked = when_the_lead_creates(&world, &stacked);

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&base, &stacked]);

    assert_eq!(
        states,
        vec!["completed".to_string(), "completed".to_string()]
    );
    assert_eq!(exit, Some(0));
    let base_branch = then_the_branch_has_the_agents_commit(&world, &base, 0);
    let stacked_branch = then_the_branch_has_the_agents_commit(&world, &stacked, 0);
    assert_ne!(base_branch, stacked_branch);
    let base_head = branch_head(&world, &base_branch);
    assert!(
        is_ancestor(&world, &base_head, &stacked_branch),
        "{stacked_branch} has {base_head}"
    );
    let stacked_parent = world.git_ok(&["rev-parse", &format!("refs/heads/{stacked_branch}^")]);
    assert_eq!(stacked_parent, base_head);
    then_only_the_repository_is_a_worktree(&world);
}

#[test]
fn a_qa_task_tests_a_completed_task_in_claude_and_completes_without_commits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let qa = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Run the new work."));
    given_the_agent_will(&world, &qa, &[Behaviour::CommitNothing]);

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&built, &qa]);

    assert_eq!(
        states,
        vec!["completed".to_string(), "completed".to_string()]
    );
    assert_eq!(exit, Some(0));
    let runs = agent_runs_for(&world, &qa);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].harness, "claude");
    assert_eq!(runs[0].dir, worktree_dir(&world, &qa));
    let branch = then_the_branch_has_the_agents_commit(&world, &built, 0);
    assert_eq!(task_label(&world, &qa, "branch"), None);
    assert_eq!(branches(&world), vec![MAIN.to_string(), branch]);
    then_the_worktree_is_removed(&world, &qa);
    then_only_the_repository_is_a_worktree(&world);
}

#[test]
fn a_qa_agent_that_commits_fails_and_leaves_the_tested_branch_alone() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let qa = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Run the new work."));
    given_the_agent_will(&world, &qa, &[Behaviour::Commit]);

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&built, &qa]);

    assert_eq!(states, vec!["completed".to_string(), "failed".to_string()]);
    assert_eq!(exit, Some(0));
    assert_eq!(agent_runs_for(&world, &qa).len(), 1);
    let branch = then_the_branch_has_the_agents_commit(&world, &built, 0);
    assert_eq!(branches(&world), vec![MAIN.to_string(), branch]);
}

#[test]
fn the_daemon_claims_a_new_task_with_a_role_within_five_seconds() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);

    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));

    assert_ne!(when_the_task_leaves_ready_within(&world, &task, 5), "ready");
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    let log = fs::read_to_string(world.home.join("daemon.log")).expect("read daemon.log");
    for word in ["claimed", "spawned", "settled"] {
        assert!(log.contains(&format!("{word} #{task}")), "{word} in {log}");
    }
}

#[test]
fn the_daemon_claims_a_ready_task_within_five_seconds_of_its_role_being_set() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut new = an_implement_task(&world, "Add one work file.");
    new.role = None;
    let task = when_the_lead_creates(&world, &new);
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_task_leaves_ready_within(&world, &task, 2), "ready");

    world.vb_lead(&["label", &task, "role=implement"], None);

    assert_ne!(when_the_task_leaves_ready_within(&world, &task, 5), "ready");
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_second_daemon_or_pool_on_the_same_home_exits_naming_the_holder() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let holder = world
        .pool
        .as_ref()
        .expect("the daemon")
        .child
        .id()
        .to_string();

    let daemon = when_rider_runs(&world, &["daemon"]);
    let pool = when_rider_runs(&world, &["pool", PROJECT]);

    for second in [daemon, pool] {
        assert_eq!(second.code, Some(1), "{second:?}");
        assert!(
            second.stderr.contains(&format!("pid {holder}")),
            "{second:?}"
        );
    }
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn sigterm_drains_the_running_agent_claims_nothing_new_and_exits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let running = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &running, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &running);

    when_the_daemon_is_sent_sigterm(&world);
    let later = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
    assert_eq!(
        when_the_task_leaves_ready_within(&world, &later, 3),
        "ready"
    );
    world.release_agent(&running);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &running), "completed");
    assert_eq!(task_state(&world, &later), "ready");
    assert!(agent_runs_for(&world, &later).is_empty());
}

#[test]
fn a_second_sigterm_stops_the_agent_and_leaves_its_task_ready_with_attempt_unchanged() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    let run = when_the_agent_has_started(&world, &task);

    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(!is_alive(run.pid), "the agent {} still runs", run.pid);
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_label(&world, &task, "attempt"), None);
}
