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

use std::path::PathBuf;

use serde_json::json;

use fake_agent::Behaviour;
use steps::*;
use world::{MAIN, World};

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
    let resumed = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, true),
    );
    let fresh = a_fix(&world, "Fix the work again.", &built, &resumed, false);
    let fresh = when_the_lead_creates(&world, &fresh);

    let tasks = [built.as_str(), resumed.as_str(), fresh.as_str()];
    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed".to_string(); 3]);
    assert_eq!(exit, Some(0));
    assert_eq!(
        fake_agent::options(&world.root, &built, 0),
        claude_options(None)
    );
    let built_session = fake_agent::session(&built, 0);
    assert_eq!(
        fake_agent::options(&world.root, &resumed, 0),
        claude_options(Some(&built_session))
    );
    assert_eq!(
        fake_agent::options(&world.root, &fresh, 0),
        claude_options(None)
    );
}

#[test]
fn a_resumed_fix_forks_the_session_so_its_onto_tasks_session_stays_its_own() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let first = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, true),
    );
    let chained = a_fix(&world, "Fix the fix.", &first, &first, true);
    let chained = when_the_lead_creates(&world, &chained);
    let again = a_fix(&world, "Fix the work again.", &built, &chained, true);
    let again = when_the_lead_creates(&world, &again);

    let tasks = [
        built.as_str(),
        first.as_str(),
        chained.as_str(),
        again.as_str(),
    ];
    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed".to_string(); 4]);
    assert_eq!(exit, Some(0));
    let built_session = fake_agent::session(&built, 0);
    let first_session = fake_agent::session(&first, 0);
    let expected = [
        (&first, claude_options(Some(&built_session))),
        (&chained, claude_options(Some(&first_session))),
        (&again, claude_options(Some(&built_session))),
    ];
    for (task, options) in expected {
        assert_eq!(
            fake_agent::options(&world.root, task, 0),
            options,
            "task {task}"
        );
    }
}

#[test]
fn a_resumed_session_claude_refuses_falls_back_to_a_fresh_start_in_the_same_attempt() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, true),
    );
    given_the_agent_will(&world, &fix, &[Behaviour::RefuseResume]);

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&built, &fix]);

    assert_eq!(states, vec!["completed".to_string(); 2]);
    assert_eq!(exit, Some(0));
    let built_session = fake_agent::session(&built, 0);
    assert_eq!(
        fake_agent::options(&world.root, &fix, 0),
        claude_options(Some(&built_session))
    );
    assert_eq!(
        fake_agent::options(&world.root, &fix, 1),
        claude_options(None)
    );
    assert_eq!(agent_runs_for(&world, &fix).len(), 2);
    assert_eq!(task_label(&world, &fix, "attempt"), None);
    then_the_branch_has_the_agents_commit(&world, &fix, 1);
    let replies = task_replies(&world, &fix)
        .iter()
        .map(|reply| reply.to_string())
        .collect();
    let replies: Vec<String> = replies;
    let fallback = replies
        .iter()
        .find(|reply| reply.contains("starting fresh"));
    assert!(
        fallback.is_some_and(|reply| reply.contains(&built_session)),
        "{replies:?}"
    );
}

#[test]
fn a_session_recorded_for_another_repository_is_not_resumed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    when_the_pool_starts(&mut world);
    assert_eq!(
        when_the_tasks_settle(&world, &[&built]),
        vec!["completed".to_string()]
    );
    let branch = task_label(&world, &built, "branch").expect("the built task's branch");
    let other = given_a_clone_with_the_branch(&world, &branch);
    let other_label = other.to_str().expect("UTF-8 path");
    let fix = a_fix(&world, "Fix the work.", &built, &built, true).label("repo", other_label);
    let fix = when_the_lead_creates(&world, &fix);

    let states = when_the_tasks_settle(&world, &[&fix]);
    let exit = when_the_pool_is_stopped(&mut world);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(exit, Some(0));
    assert_eq!(
        fake_agent::options(&world.root, &fix, 0),
        claude_options(None)
    );
    let run = &agent_runs_for(&world, &fix)[0];
    assert!(run.dir.ends_with(format!("other-repo/{fix}")), "{run:?}");
}

/// A fix of task `onto` that waits for `after`, resuming `onto`'s session when `resume`.
fn a_fix(world: &World, text: &str, onto: &str, after: &str, resume: bool) -> NewTask {
    let fix = NewTask::new(text)
        .label("repo", world.repo_label())
        .label("onto", onto)
        .needs(after)
        .role("implement");
    if resume {
        fix.label("resume", "1")
    } else {
        fix
    }
}

/// A clone of the world's repository with `branch` as a local branch, ready for commits.
fn given_a_clone_with_the_branch(world: &World, branch: &str) -> PathBuf {
    let other = world.root.join("other-repo");
    let repo = world.repo_label();
    let other_label = other.to_str().expect("UTF-8 path");
    let clone = world.git(&world.root, &["clone", "-q", repo, other_label]);
    assert_eq!(clone.code, Some(0), "git clone: {clone:?}");
    let remote = format!("origin/{branch}");
    for args in [
        &["branch", branch, &remote][..],
        &["config", "user.name", "Scenario Tester"],
        &["config", "user.email", "scenario@example.invalid"],
        &["config", "commit.gpgsign", "false"],
    ] {
        assert_eq!(world.git(&other, args).code, Some(0), "git {args:?}");
    }
    other
}

/// The options rider starts the implement role's `claude` with, resuming `session` if given.
fn claude_options(session: Option<&str>) -> Vec<String> {
    let mut options: Vec<String> = [
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
    if let Some(session) = session {
        options.extend(["--resume", session, "--fork-session"].map(String::from));
    }
    options
}
