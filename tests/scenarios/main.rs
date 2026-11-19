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

use serde_json::json;

use fake_agent::Behaviour;
use steps::*;

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
    let stacked = steps::NewTask::new("Add the stacked work.")
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
}
