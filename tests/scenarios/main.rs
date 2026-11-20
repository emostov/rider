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
use world::MAIN;

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
fn a_fix_task_with_resume_continues_the_claude_session_of_its_onto_task() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = |text: &str, after: &str| {
        NewTask::new(text)
            .label("repo", world.repo_label())
            .label("onto", &built)
            .needs(after)
            .role("implement")
    };
    let resumed = when_the_lead_creates(&world, &fix("Fix the work.", &built).label("resume", "1"));
    let fresh = when_the_lead_creates(&world, &fix("Fix the work again.", &resumed));

    let tasks = [built.as_str(), resumed.as_str(), fresh.as_str()];
    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed".to_string(); 3]);
    assert_eq!(exit, Some(0));
    let fresh_options: Vec<String> = [
        "-p",
        "--model",
        "claude-opus-5-5",
        "--output-format",
        "stream-json",
        "--verbose",
        "--dangerously-skip-permissions",
    ]
    .map(String::from)
    .into();
    let mut resumed_options = fresh_options.clone();
    resumed_options.extend(["--resume".to_string(), fake_agent::session(&built, 0)]);
    assert_eq!(fake_agent::options(&world.root, &built, 0), fresh_options);
    assert_eq!(
        fake_agent::options(&world.root, &resumed, 0),
        resumed_options
    );
    assert_eq!(fake_agent::options(&world.root, &fresh, 0), fresh_options);
}
