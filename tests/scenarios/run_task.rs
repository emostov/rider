use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{MAIN, World};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

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
    assert_eq!(
        block,
        ("attempts_exhausted".to_string(), reason.to_string())
    );
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

fn when_it_fails_and_is_reopened(world: &mut World, task: &str) -> Vec<String> {
    given_a_running_daemon(world);
    let first = when_the_tasks_settle(world, &[task]);
    world.vb_lead(&["tasks", "reopen", task], None);
    let second = when_the_tasks_settle(world, &[task]);
    when_the_daemon_is_sent_sigterm(world);
    assert_eq!(then_the_daemon_exits(world), Some(0));
    [first, second].concat()
}

fn then_the_retry_is_blocked_off_its_branch(behave: Behaviour, on: &str) {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[behave]);
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["failed"]);
    let branch = task_label(&world, &task, "branch").expect("the task's branch");
    let text = task_text(&world, &task);
    let off = format!("no committed work: the worktree is not on its branch {branch}");
    assert!(text.contains(&off), "{text}");

    world.vb_lead(&["tasks", "reopen", &task], None);
    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    let exit = then_the_daemon_exits(&mut world);
    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let reason = format!(
        "Rider cannot prepare the worktree: {} is on {on}, not on branch {branch}.",
        worktree_dir(&world, &task).display()
    );
    assert_eq!(
        task_block(&world, &task),
        Some(("rider_error".to_string(), reason))
    );
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
}

#[test]
fn an_agent_that_commits_on_another_branch_fails_and_its_retry_is_blocked() {
    then_the_retry_is_blocked_off_its_branch(Behaviour::WanderBranch, "branch elsewhere");
}

#[test]
fn an_agent_that_commits_on_a_detached_head_fails_and_its_retry_is_blocked() {
    then_the_retry_is_blocked_off_its_branch(Behaviour::Detach, "a detached HEAD");
}

#[test]
fn a_retry_completes_on_the_work_an_earlier_failed_attempt_committed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(
        &world,
        &task,
        &[Behaviour::CommitThenFail, Behaviour::CommitNothing],
    );

    let states = when_it_fails_and_is_reopened(&mut world, &task);

    assert_eq!(states, vec!["failed", "completed"]);
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    let record = json!({
        "branch": branch,
        "head": branch_head(&world, &branch),
        "commits": 1,
        "summary": fake_agent::summary(&task, 1),
        "behind": [],
    });
    assert_eq!(task_records(&world, &task, "rider-result"), vec![record]);
}

fn then_it_fails_with_no_commits_of_its_own(world: &mut World, task: &str) {
    let (states, exit) = when_the_daemon_runs_until_settled(world, &[task]);
    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    let text = task_text(world, task);
    let reason = "no committed work: the branch has no commits of its own";
    assert!(text.contains(reason), "{text}");
}

#[test]
fn a_malformed_start_label_is_ignored_and_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_implement_task(&world, "Add the work.").label("start", "HEAD~3");
    let task = when_the_lead_creates(&world, &task);
    given_the_agent_will(&world, &task, &[Behaviour::CommitNothing]);

    then_it_fails_with_no_commits_of_its_own(&mut world, &task);

    assert_eq!(
        task_label(&world, &task, "start"),
        Some("HEAD~3".to_string())
    );
}

#[test]
fn an_older_start_label_does_not_count_main_moving_on_as_the_tasks_work() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let root = world.git_ok(&["rev-parse", MAIN]);
    world.git_ok(&["commit", "-q", "--allow-empty", "-m", "Move main on."]);
    let task = an_implement_task(&world, "Add the work.").label("start", &root);
    let task = when_the_lead_creates(&world, &task);
    given_the_agent_will(&world, &task, &[Behaviour::CommitNothing]);

    then_it_fails_with_no_commits_of_its_own(&mut world, &task);
}

#[test]
fn a_fast_forward_onto_another_branch_is_not_work_of_its_own() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tree = format!("{MAIN}^{{tree}}");
    let sibling = world.git_ok(&["commit-tree", &tree, "-p", MAIN, "-m", "Sibling work."]);
    world.git_ok(&["branch", "sibling", &sibling]);
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::FastForwardSibling]);

    then_it_fails_with_no_commits_of_its_own(&mut world, &task);
}

#[test]
fn a_start_label_naming_no_commit_here_is_ignored() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let missing = "0".repeat(40);
    let task = an_implement_task(&world, "Add the work.").label("start", &missing);
    let task = when_the_lead_creates(&world, &task);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    let records = task_records(&world, &task, "rider-result");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["branch"], json!(branch));
    assert_eq!(records[0]["commits"], json!(1));
    assert_eq!(task_label(&world, &task, "start"), Some(missing));
}

#[test]
fn a_restack_that_only_drops_commits_fails() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let restack =
        crate::a_fix(&world, "Restack the work.", &built, &built, false).label("restack", "1");
    let restack = when_the_lead_creates(&world, &restack);
    given_the_agent_will(&world, &restack, &[Behaviour::DropOnly]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &restack]);

    assert_eq!(
        (states, exit),
        (vec!["completed".to_string(), "failed".to_string()], Some(0))
    );
    let text = task_text(&world, &restack);
    let reason = "no committed work: the branch has no commits of its own";
    assert!(text.contains(reason), "{text}");
}

#[test]
fn a_worktree_with_a_bisect_in_progress_is_kept_and_the_task_completes() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::CommitThenBisect]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    let dir = worktree_dir(&world, &task);
    let kept = format!(
        "Kept the worktree at {dir}, so branch {branch} stays checked out there: tasks \
         working `onto` it will block as branch_busy until a person removes or fixes the \
         worktree at {dir}. It was kept because an operation is in progress.",
        dir = dir.display()
    );
    let text = task_text(&world, &task);
    assert!(text.contains(&kept), "{text}");
    assert!(dir.join(fake_agent::work_file(&task, 0)).is_file());
}

#[test]
fn an_onto_task_runs_on_a_branch_whose_worktree_held_only_ignored_build_output() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &built, &[Behaviour::IgnoredOutput]);
    let fix = crate::a_fix(&world, "Fix the work.", &built, &built, false);
    let fix = when_the_lead_creates(&world, &fix);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &fix]);

    assert_eq!(
        (states, exit),
        (
            vec!["completed".to_string(), "completed".to_string()],
            Some(0)
        )
    );
    let branch = task_label(&world, &built, "branch").expect("the built task's branch");
    assert_eq!(
        branch_head_subject(&world, &branch),
        fake_agent::work_subject(&fix, 0)
    );
}

#[test]
fn a_blocked_then_unblocked_attempt_keeps_the_log_of_each_run() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Block, Behaviour::Commit]);
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["blocked"]);

    world.vb_lead(&["tasks", "unblock", &task], None);
    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (vec!["completed".to_string()], Some(0))
    );
    assert_eq!(
        task_logs(&world, &task),
        vec!["1-2.jsonl", "1.jsonl", "session"]
    );
}

fn when_it_fails_its_log_reply(world: &mut World, behave: Behaviour) -> String {
    let task = when_the_lead_creates(world, &an_implement_task(world, "Add the work."));
    given_the_agent_will(world, &task, &[behave]);

    let (states, exit) = when_the_daemon_runs_until_settled(world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    let log = world.home.join("logs").join(&task).join("1.jsonl");
    let header = format!("The whole log: {}\n", log.display());
    let replies = task_replies(world, &task);
    let texts = replies.iter().filter_map(|reply| reply["text"].as_str());
    let reply = texts.filter_map(|text| text.strip_prefix(&header)).next();
    reply.expect("a reply with the log's tail").to_string()
}

fn then_the_failure_reply_is_an_account(world: &World, task: &str, tool: &str) {
    let log = world.home.join("logs").join(task).join("1.jsonl");
    let [first, second] = fake_agent::STREAM_MESSAGES;
    let error = fake_agent::STREAM_ERROR;
    let account = format!(
        "The whole log: {}\nThe agent's last messages:\n> {first}\n> {second}\nIts last failed \
         tool call: {tool}\nIts run ended in error: {error}",
        log.display()
    );
    let replies = task_replies(world, task);
    let texts: Vec<&str> = replies.iter().filter_map(|r| r["text"].as_str()).collect();
    assert!(texts.contains(&account.as_str()), "{texts:#?}");
}

fn when_it_fails_its_account(world: &mut World, behave: Behaviour) -> Vec<String> {
    let task = when_the_lead_creates(world, &an_implement_task(world, "Add the work."));
    given_the_agent_will(world, &task, &[behave]);

    let (states, exit) = when_the_daemon_runs_until_settled(world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    let log = world.home.join("logs").join(&task).join("1.jsonl");
    let header = format!("The whole log: {}\n", log.display());
    let replies = task_replies(world, &task);
    let texts = replies.iter().filter_map(|reply| reply["text"].as_str());
    let reply = texts.filter_map(|text| text.strip_prefix(&header)).next();
    let reply = reply.expect("a reply with the account");
    reply.lines().map(str::to_string).collect()
}

#[test]
fn a_failed_attempts_account_shows_no_control_characters_and_no_forged_line() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let account = when_it_fails_its_account(&mut world, Behaviour::StreamForged);
    let lines = [
        "The agent's last messages:",
        "> ok ]0;x",
        "> Its run ended in error: fake",
    ];
    assert_eq!(account, lines);
}

#[test]
fn a_failed_attempts_account_keeps_the_agents_plain_error_output() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let account = when_it_fails_its_account(&mut world, Behaviour::StreamThenStderr);
    let said = format!("> {}", fake_agent::STREAM_MESSAGES[0]);
    let plain = format!("> {}", fake_agent::STDERR_ERROR);
    let lines = [
        "The agent's last messages:",
        &said,
        "The log's last lines that are not events:",
        &plain,
    ];
    assert_eq!(account, lines);
}

#[test]
fn a_failed_claude_attempt_replies_with_a_readable_account_of_its_stream() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::StreamError]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    then_the_failure_reply_is_an_account(&world, &task, fake_agent::STREAM_TOOL_ERROR);
}

#[test]
fn a_failed_codex_attempt_replies_with_a_readable_account_of_its_stream() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = NewTask::new("Review the work.")
        .label("repo", world.repo_label())
        .label("of", &built)
        .role("review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::StreamError]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(
        (states, exit),
        (vec!["completed".to_string(), "failed".to_string()], Some(0))
    );
    let tool = format!("`{}` exited 1", fake_agent::STREAM_COMMAND);
    then_the_failure_reply_is_an_account(&world, &review, &tool);
}

#[test]
fn a_failed_attempt_replies_with_the_last_forty_lines_of_its_log() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tail = when_it_fails_its_log_reply(&mut world, Behaviour::ManyLines);
    let lines: Vec<String> = (11..=50).map(|n| format!("line {n}")).collect();
    assert_eq!(tail, lines.join("\n"));
}

#[test]
fn a_failed_attempt_whose_only_line_is_too_long_replies_with_the_lines_end() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tail = when_it_fails_its_log_reply(&mut world, Behaviour::LongLine);
    assert!(
        tail.len() > 1000 && tail.chars().all(|c| c == 'z'),
        "{tail}"
    );
}

#[test]
fn a_task_based_on_a_task_without_a_branch_is_blocked_as_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let base = when_the_lead_creates(&world, &NewTask::new("Done by hand."));
    given_the_lead_completes(&world, &base);
    let task = NewTask::new("Add the work.")
        .label("repo", world.repo_label())
        .label("base", &base)
        .needs(&base)
        .role("implement");
    let task = when_the_lead_creates(&world, &task);

    let block = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    let reason = format!(
        "Set the `branch` label on task #{base} to its branch, or remove the `base` label \
         from this task."
    );
    assert_eq!(block, ("invalid_brief".to_string(), reason));
}

#[test]
fn a_completed_task_whose_record_cannot_be_posted_replies_with_it_instead() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_vb_fails_every(&world, &[r#""post "[0-9]*"#]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    assert_eq!(
        task_records(&world, &task, "rider-result"),
        Vec::<Value>::new()
    );
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    let record = json!({
        "branch": branch,
        "head": branch_head(&world, &branch),
        "commits": 1,
        "summary": fake_agent::summary(&task, 0),
        "behind": [],
    });
    let replies = task_replies(&world, &task);
    let texts = replies.iter().filter_map(|reply| reply["text"].as_str());
    let records: Vec<Value> = texts
        .filter_map(|text| serde_json::from_str(text).ok())
        .collect();
    assert_eq!(records, vec![record]);
}

#[test]
fn a_failed_attempt_whose_log_reply_cannot_be_posted_still_counts_its_attempt() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::ExitNonZero]);
    given_vb_fails_the_first_reply(&world);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let log = world.home.join("logs").join(&task).join("1.jsonl");
    let header = format!("The whole log: {}", log.display());
    let replies = task_replies(&world, &task);
    let texts: Vec<&str> = replies.iter().filter_map(|r| r["text"].as_str()).collect();
    assert_eq!(texts, vec![header.as_str()]);
}

#[test]
fn a_completed_task_whose_record_and_its_reply_both_fail_still_releases_its_worktree() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_vb_fails_every(&world, &[r#""post "[0-9]*"#, r#""reply "*"#]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    then_the_worktree_is_removed(&world, &task);
    assert_eq!(
        task_records(&world, &task, "rider-result"),
        Vec::<Value>::new()
    );
    assert_eq!(task_replies(&world, &task), Vec::<Value>::new());
}

#[test]
fn a_failed_attempt_writes_no_attempt_label_or_log_reply_before_its_fail_lands() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::ExitNonZero]);
    let (marker, release) = (world.root.join("failing"), world.root.join("release"));
    given_vb_waits_for(&world, "fail", &marker, &release);
    given_a_running_daemon(&mut world);
    when_the_file_exists(&world, &marker);

    let (label, replies) = (
        task_label(&world, &task, "attempt"),
        task_replies(&world, &task),
    );
    std::fs::write(&release, "").expect("release the held fail");
    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(states, vec!["failed"]);
    assert_eq!((label, replies), (None, Vec::<Value>::new()));
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
}

#[test]
fn an_agent_cli_missing_from_path_fails_its_attempt_replying_with_the_error() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    std::fs::remove_file(world.agents.join("claude")).expect("remove the fake claude");

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let text = task_text(&world, &task);
    assert!(text.contains("agent exited 127"), "{text}");
    assert!(text.contains("claude: not found"), "{text}");
}

#[test]
fn a_role_prompt_file_removed_while_the_daemon_runs_blocks_its_task_as_a_rider_error() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let prompt = world.root.join("docs-prompt.md");
    std::fs::write(&prompt, "You write the documentation for one task.\n").expect("a prompt");
    let toml = format!(
        r#"[roles.docs]
harness = "claude"
model = "claude-docs-model"
title = "Docs"
model_name = "Docs Model"
model_slug = "docs-model"
prompt = "{}"
commits = true
"#,
        prompt.display()
    );
    given_a_rider_toml(&world, &toml);
    given_a_running_daemon(&mut world);
    std::fs::remove_file(&prompt).expect("remove the prompt");
    let mut task = an_implement_task(&world, "Document the work.");
    task.role = Some("docs".to_string());
    let task = when_the_lead_creates(&world, &task);

    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (vec!["blocked".to_string()], Some(0))
    );
    assert_eq!(agent_runs(&world), vec![]);
    assert!(!worktree_dir(&world, &task).exists());
    let reason = format!(
        "Rider cannot read its prompt file {}: No such file or directory (os error 2).",
        prompt.display()
    );
    assert_eq!(
        task_block(&world, &task),
        Some(("rider_error".to_string(), reason))
    );
}

#[test]
fn an_attempt_whose_log_cannot_be_created_fails_naming_the_error() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let dir = world.home.join("logs").join(&task);
    std::fs::create_dir_all(&dir).expect("the task's log directory");
    let read_only = std::fs::Permissions::from_mode(0o555);
    std::fs::set_permissions(&dir, read_only).expect("make the log directory read-only");

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    assert_eq!(agent_runs(&world), vec![]);
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let text = task_text(&world, &task);
    let reason = format!("the agent's log failed: {}", dir.join("1.jsonl").display());
    assert!(text.contains(&reason), "{text}");
}

#[test]
fn a_task_whose_board_read_fails_is_blocked_naming_the_error_and_no_agent_runs() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let read = format!(r#""read {task} --json --url "*" --as implement"*"#);
    given_vb_fails_every(&world, &[&read]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    assert_eq!(agent_runs(&world), vec![]);
    assert!(!worktree_dir(&world, &task).exists());
    assert_eq!(task_label(&world, &task, "branch"), None);
    let reason = "Rider could not prepare the attempt: vb exited with code 1: injected read \
                  failure.";
    assert_eq!(
        task_block(&world, &task),
        Some(("rider_error".to_string(), reason.to_string()))
    );
}

#[test]
fn an_attempt_run_past_its_log_bound_reuses_its_last_log() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let dir = world.home.join("logs").join(&task);
    std::fs::create_dir_all(&dir).expect("the task's log directory");
    let mut names = vec!["1.jsonl".to_string()];
    names.extend((2..=16).map(|run| format!("1-{run}.jsonl")));
    for name in &names {
        std::fs::write(dir.join(name), "an earlier run\n").expect("an earlier log");
    }

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    names.push("session".to_string());
    names.sort();
    assert_eq!(task_logs(&world, &task), names);
    let last = std::fs::read_to_string(dir.join("1-16.jsonl")).expect("the last log");
    assert!(last.contains(&fake_agent::summary(&task, 0)), "{last}");
    let first = std::fs::read_to_string(dir.join("1-15.jsonl")).expect("an earlier log");
    assert_eq!(first, "an earlier run\n");
}

#[test]
fn a_failed_attempt_replies_marking_the_bytes_of_its_log_that_are_not_utf8() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tail = when_it_fails_its_log_reply(&mut world, Behaviour::BadBytes);
    assert_eq!(tail, "bad \u{fffd} byte\nend");
}

#[test]
fn a_failed_attempt_replies_with_the_whole_lines_of_its_logs_last_16_kib() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tail = when_it_fails_its_log_reply(&mut world, Behaviour::WideLines);
    let line = "\u{1f600}".repeat(250);
    assert_eq!(tail, vec![line.as_str(); 16].join("\n"));
}

fn then_a_task_of_a_task_without_a_branch_is_blocked(key: &str, role: &str, reason: &str) {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let other = when_the_lead_creates(&world, &NewTask::new("Done by hand."));
    given_the_lead_completes(&world, &other);
    let task = NewTask::new("Work on it.")
        .label("repo", world.repo_label())
        .label(key, &other)
        .needs(&other)
        .role(role);
    let task = when_the_lead_creates(&world, &task);

    let block = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    let reason = reason.replace("{other}", &other);
    assert_eq!(block, ("invalid_brief".to_string(), reason));
}

#[test]
fn a_task_onto_a_task_without_a_branch_is_blocked_as_an_invalid_brief() {
    then_a_task_of_a_task_without_a_branch_is_blocked(
        "onto",
        "implement",
        "Set the `branch` label on task #{other} to its branch, or remove the `onto` label \
         from this task.",
    );
}

#[test]
fn a_review_of_a_task_without_a_branch_is_blocked_as_an_invalid_brief() {
    then_a_task_of_a_task_without_a_branch_is_blocked(
        "of",
        "review",
        "Set the `branch` label on task #{other} to the branch to check, or change the `of` \
         label to a task that has one.",
    );
}

#[test]
fn a_task_whose_worktree_path_holds_a_plain_directory_is_blocked_as_a_rider_error() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let dir = worktree_dir(&world, &task);
    std::fs::create_dir_all(&dir).expect("a plain directory");

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    assert_eq!(agent_runs(&world), vec![]);
    let reason = format!(
        "Rider cannot prepare the worktree: {} exists but is not a checkout of the repository.",
        dir.display()
    );
    assert_eq!(
        task_block(&world, &task),
        Some(("rider_error".to_string(), reason))
    );
}

fn given_a_checkout_elsewhere_of(world: &World, branch: &str) {
    let dir = world.root.join("elsewhere");
    let dir = dir.to_str().expect("a UTF-8 path");
    let added = world.git(&world.repo, &["worktree", "add", "-q", dir, branch]);
    assert_eq!(added.code, Some(0), "{added:?}");
}

#[test]
fn a_task_onto_a_branch_checked_out_elsewhere_is_blocked_as_branch_busy() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &[&built]);
    assert_eq!(states, vec!["completed"]);
    let branch = task_label(&world, &built, "branch").expect("the built task's branch");
    given_a_checkout_elsewhere_of(&world, &branch);
    let fix = NewTask::new("Fix the work.")
        .label("repo", world.repo_label())
        .label("onto", &built)
        .needs(&built)
        .role("implement");
    let fix = when_the_lead_creates(&world, &fix);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&fix]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    assert_eq!(agent_runs_for(&world, &fix), vec![]);
    let reason = format!(
        "Wait until the branch of task #{built} is not checked out in another worktree, then \
         unblock this task."
    );
    assert_eq!(
        task_block(&world, &fix),
        Some(("branch_busy".to_string(), reason))
    );
}

#[test]
fn a_task_whose_branch_is_checked_out_elsewhere_is_blocked_as_branch_busy() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    world.git_ok(&["branch", "rider/busy"]);
    given_a_checkout_elsewhere_of(&world, "rider/busy");
    let task = an_implement_task(&world, "Add the work.").label("branch", "rider/busy");
    let task = when_the_lead_creates(&world, &task);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    assert_eq!(agent_runs(&world), vec![]);
    let reason = "Wait until branch rider/busy is not checked out in another worktree, then \
                  unblock this task.";
    assert_eq!(
        task_block(&world, &task),
        Some(("branch_busy".to_string(), reason.to_string()))
    );
}

#[test]
fn a_task_canceled_before_rider_first_reads_it_gets_no_writes_and_no_agent() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let release = world.root.join("release");
    given_vb_holds_the_first_read_of(&world, &[&task], &release);
    given_a_running_daemon(&mut world);
    when_the_file_exists(&world, &world.root.join(format!("marker-{task}")));

    world.vb_lead(&["tasks", "cancel", &task, "superseded", "--json"], None);
    let (replies, labels) = (task_replies(&world, &task), task_labels(&world, &task));
    assert!(when_rider_status_shows_within(&world, "1 of 32 running", 1));
    std::fs::write(&release, "").expect("release the held read");
    assert!(
        when_rider_status_shows_within(&world, "0 of 32 running", SETTLE_SECONDS_MAX),
        "the run never ended:\n{}",
        world.daemon_log()
    );
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &task), "canceled");
    assert_eq!(task_replies(&world, &task), replies);
    assert_eq!(task_labels(&world, &task), labels);
    assert_eq!(agent_runs(&world), vec![]);
    assert!(!worktree_dir(&world, &task).exists());
}

#[test]
fn a_fail_that_fails_twice_is_retried_and_counts_the_attempt_and_replies_its_log_once() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::ExitNonZero]);
    let fails = world.root.join("fails");
    given_vb_fails_the_first(&world, "fail", 2, &fails);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    let fails = std::fs::read_to_string(&fails).expect("the logged fail calls");
    assert_eq!(fails, "fail\nfail\nfail\n");
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let text = task_text(&world, &task);
    assert_eq!(text.matches("The whole log: ").count(), 1, "{text}");
    assert_eq!(agent_runs(&world).len(), 1);
}

#[test]
fn a_task_from_a_missing_branch_is_blocked_naming_the_git_command_that_failed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = NewTask::new("Add the work.")
        .label("repo", world.repo_label())
        .label("from", "missing-branch")
        .role("implement");
    let task = when_the_lead_creates(&world, &task);

    let (kind, reason) = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    let reason_wanted = "Rider cannot prepare the worktree: git rev-parse --verify --end-of-options \
                         missing-branch^{commit} failed: fatal: Needed a single revision.";
    assert_eq!(
        (kind.as_str(), reason.as_str()),
        ("rider_error", reason_wanted)
    );
}

#[test]
fn a_board_read_failing_while_the_worktree_is_prepared_blocks_with_vbs_error_cut_short() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_vb_fails_later_reads_of(&world, &task, 200);

    let (kind, reason) = then_it_is_blocked_before_any_agent_runs(&mut world, &task);

    let words = "injected ".repeat(200);
    let uncut = format!("Rider cannot prepare the worktree: vb exited with code 1: {words}");
    let wanted: String = uncut.chars().take(500).collect();
    assert_eq!((kind, reason), ("rider_error".to_string(), wanted));
}

#[test]
fn a_task_onto_a_task_with_too_many_labels_is_blocked_as_a_rider_error() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut crowded = NewTask::new("Hold many labels.");
    for n in 0..33 {
        crowded = crowded.label(&format!("k{n}"), "v");
    }
    let crowded = when_the_lead_creates(&world, &crowded);
    given_the_lead_completes(&world, &crowded);
    let fix = NewTask::new("Fix the work.")
        .label("repo", world.repo_label())
        .label("onto", &crowded)
        .needs(&crowded)
        .role("implement");
    let fix = when_the_lead_creates(&world, &fix);

    let block = then_it_is_blocked_before_any_agent_runs(&mut world, &fix);

    let reason = "Rider could not prepare the attempt: the task has more than 32 labels.";
    assert_eq!(block, ("rider_error".to_string(), reason.to_string()));
}
