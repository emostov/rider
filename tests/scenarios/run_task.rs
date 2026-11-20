use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{MAIN, World};

fn then_it_is_blocked_before_any_agent_runs(world: &mut World, task: &str) -> (String, String) {
    let (states, exit) = when_the_daemon_runs_until_settled(world, &[task]);
    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    assert_eq!(agent_runs(world), vec![]);
    assert_eq!(branches(world), vec![MAIN.to_string()]);
    assert!(!worktree_dir(world, task).exists());
    task_block(world, task).expect("a blocked task has a block")
}

#[test]
fn a_task_with_a_malformed_attempt_label_is_blocked_as_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_implement_task(&world, "Add the work.").label("attempt", "two");
    let task = when_the_lead_creates(&world, &task);

    let block = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    let reason = "Change the `attempt` label from \"two\" to the number of failed attempts so \
                  far, such as 1.";
    assert_eq!(block, ("invalid_brief".to_string(), reason.to_string()));
}

#[test]
fn a_task_that_failed_every_attempt_is_blocked_as_attempts_exhausted() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_implement_task(&world, "Add the work.").label("attempt", "3");
    let task = when_the_lead_creates(&world, &task);

    let block = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    let reason = "The task has failed 3 attempts; change its brief or role and set the \
                  `attempt` label to 0 before reopening it.";
    assert_eq!(block, ("attempts_exhausted".to_string(), reason.to_string()));
}

#[test]
fn a_task_without_from_in_a_repository_without_a_remote_is_blocked_as_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = NewTask::new("Add the work.")
        .label("repo", world.repo_label())
        .role("implement");
    let task = when_the_lead_creates(&world, &task);

    let (kind, reason) = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    assert_eq!(kind, "invalid_brief");
    assert!(reason.contains("`from=<branch>`"), "{reason}");
}

#[test]
fn a_task_whose_branch_label_names_another_branch_is_blocked_and_the_label_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_implement_task(&world, "Add the work.").label("branch", "rider/old..work");
    let task = when_the_lead_creates(&world, &task);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    assert_eq!(agent_runs(&world), vec![]);
    let (kind, reason) = task_block(&world, &task).expect("a blocked task has a block");
    assert_eq!(kind, "rider_error");
    let named = "The task's `branch` label names \"rider/old..work\" but its worktree is on \
                 branch ";
    assert!(reason.starts_with(named), "{reason}");
    assert_eq!(
        task_label(&world, &task, "branch"),
        Some("rider/old..work".to_string())
    );
}

#[test]
fn a_failed_attempt_counts_its_attempt_and_replies_with_its_log_keeping_the_worktree() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_implement_task(&world, "Add the work.").label("attempt", "1");
    let task = when_the_lead_creates(&world, &task);
    given_the_agent_will(&world, &task, &[Behaviour::ExitNonZero]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    assert_eq!(task_label(&world, &task, "attempt"), Some("2".to_string()));
    let text = task_text(&world, &task);
    let reason = format!("agent exited {}", fake_agent::FAIL_EXIT_CODE);
    assert!(text.contains(&reason), "{text}");
    let log = world.home.join("logs").join(&task).join("2.jsonl");
    let log = format!("The whole log: {}", log.display());
    assert!(text.contains(&log), "{text}");
    assert!(worktree_dir(&world, &task).exists());
}

#[test]
fn a_review_that_commits_fails_as_having_changed_the_checkout() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = NewTask::new("Review the work.")
        .label("repo", world.repo_label())
        .label("of", &built)
        .needs(&built)
        .role("review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::Commit]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "failed"]);
    assert_eq!(exit, Some(0));
    let text = task_text(&world, &review);
    assert!(text.contains("review changed the checkout"), "{text}");
    let branch = then_the_branch_has_the_agents_commit(&world, &built, 0);
    assert_eq!(branches(&world), vec![MAIN.to_string(), branch]);
}
