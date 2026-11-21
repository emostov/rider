use std::thread;
use std::time::Duration;

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::World;

const REVIEW_MODEL: &str = "gpt-6.1-sol";

#[test]
fn a_follow_up_review_resumes_the_codex_session_of_its_onto_review_in_its_of_worktree() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let first = when_the_lead_creates(&world, &a_review(&world, "Review the work.", &built));
    let follow_up = a_review(&world, "Check the requests were addressed.", &built)
        .label("onto", &first)
        .label("resume", "1")
        .needs(&first);
    let follow_up = when_the_lead_creates(&world, &follow_up);
    given_the_agent_will(&world, &first, &[Behaviour::CommitNothing]);
    given_the_agent_will(&world, &follow_up, &[Behaviour::CommitNothing]);

    let tasks = [built.as_str(), first.as_str(), follow_up.as_str()];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed".to_string(); 3]);
    assert_eq!(exit, Some(0));
    let first_dir = worktree_dir(&world, &first);
    assert_eq!(
        fake_agent::options(&world.root, &first, 0),
        codex_options(&first_dir)
    );
    let session = fake_agent::session(&first, 0);
    assert_eq!(
        fake_agent::options(&world.root, &follow_up, 0),
        codex_resume_options()
    );
    assert_eq!(resumed_session(&world, &follow_up, 0), Some(session));
    let runs = agent_runs_for(&world, &follow_up);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].dir, worktree_dir(&world, &follow_up));
    let note = format!("Resuming the Codex session of task #{first}.");
    assert!(replies(&world, &follow_up).contains(&note), "{note}");
}

#[test]
fn a_follow_up_review_onto_a_claude_session_starts_fresh_and_says_why() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = a_review(&world, "Review the work.", &built)
        .label("onto", &built)
        .label("resume", "1");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::CommitNothing]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed".to_string(); 2]);
    assert_eq!(exit, Some(0));
    let dir = worktree_dir(&world, &review);
    assert_eq!(
        fake_agent::options(&world.root, &review, 0),
        codex_options(&dir)
    );
    let note = format!(
        "Starting a fresh session despite `resume=1`: task #{built}'s session was recorded by \
         Claude, and role review runs Codex."
    );
    assert!(replies(&world, &review).contains(&note), "{note}");
}

#[test]
fn a_follow_up_review_whose_session_codex_refuses_starts_fresh_in_the_same_attempt() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let first = when_the_lead_creates(&world, &a_review(&world, "Review the work.", &built));
    let follow_up = a_review(&world, "Check the requests were addressed.", &built)
        .label("onto", &first)
        .label("resume", "1")
        .needs(&first);
    let follow_up = when_the_lead_creates(&world, &follow_up);
    given_the_agent_will(&world, &first, &[Behaviour::CommitNothing]);
    given_the_agent_will(&world, &follow_up, &[Behaviour::RefuseResumeThenNothing]);

    let tasks = [built.as_str(), first.as_str(), follow_up.as_str()];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed".to_string(); 3]);
    assert_eq!(exit, Some(0));
    let session = fake_agent::session(&first, 0);
    assert_eq!(
        fake_agent::options(&world.root, &follow_up, 0),
        codex_resume_options()
    );
    assert_eq!(
        resumed_session(&world, &follow_up, 0),
        Some(session.clone())
    );
    let dir = worktree_dir(&world, &follow_up);
    assert_eq!(
        fake_agent::options(&world.root, &follow_up, 1),
        codex_options(&dir)
    );
    assert_eq!(agent_runs_for(&world, &follow_up).len(), 2);
    assert_eq!(task_label(&world, &follow_up, "attempt"), None);
    let note = format!(
        "Codex could not resume session {session} before doing any work, so the attempt ran \
         again starting fresh."
    );
    assert!(replies(&world, &follow_up).contains(&note), "{note}");
}

#[test]
fn a_follow_up_review_whose_codex_fails_for_another_reason_is_not_rerun_fresh() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let first = when_the_lead_creates(&world, &a_review(&world, "Review the work.", &built));
    let follow_up = a_review(&world, "Check the requests were addressed.", &built)
        .label("onto", &first)
        .label("resume", "1")
        .needs(&first);
    let follow_up = when_the_lead_creates(&world, &follow_up);
    given_the_agent_will(&world, &first, &[Behaviour::CommitNothing]);
    given_the_agent_will(&world, &follow_up, &[Behaviour::FailBeforeThread]);

    let tasks = [built.as_str(), first.as_str(), follow_up.as_str()];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed", "completed", "failed"]);
    assert_eq!(exit, Some(0));
    assert_eq!(agent_runs_for(&world, &follow_up).len(), 1);
    let session = fake_agent::session(&first, 0);
    assert_eq!(resumed_session(&world, &follow_up, 0), Some(session));
    let blamed = replies(&world, &follow_up);
    assert!(
        !blamed
            .iter()
            .any(|reply| reply.contains("could not resume")),
        "{blamed:?}"
    );
}

#[test]
fn two_follow_up_reviews_onto_one_review_never_resume_its_session_at_once() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let first = when_the_lead_creates(&world, &a_review(&world, "Review the work.", &built));
    let follow_up = |text: &str| {
        let task = a_review(&world, text, &built)
            .label("onto", &first)
            .label("resume", "1")
            .needs(&first);
        when_the_lead_creates(&world, &task)
    };
    let one = follow_up("Check the first request.");
    let two = follow_up("Check the second request.");
    given_the_agent_will(&world, &first, &[Behaviour::CommitNothing]);
    given_the_agent_will(&world, &one, &[Behaviour::HoldThenNothing]);
    given_the_agent_will(&world, &two, &[Behaviour::HoldThenNothing]);
    given_a_running_daemon(&mut world);

    when_the_agent_has_started(&world, &one);
    thread::sleep(Duration::from_secs(3));

    assert_eq!(agent_runs_for(&world, &two), vec![]);
    world.release_agent(&one);
    when_the_agent_has_started(&world, &two);
    world.release_agent(&two);
    let states = when_the_tasks_settle(&world, &[&one, &two]);
    assert_eq!(states, vec!["completed".to_string(); 2]);
}

#[test]
fn a_review_onto_another_task_without_resume_is_blocked_as_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = a_review(&world, "Review the work.", &built).label("onto", &built);
    let review = when_the_lead_creates(&world, &review);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "blocked"]);
    assert_eq!(exit, Some(0));
    assert_eq!(agent_runs_for(&world, &review), vec![]);
    let reason = format!(
        "Set the `resume` label to 1 to continue the agent session of task #{built}, or remove \
         the `onto` label: a review task works on no branch."
    );
    assert_eq!(
        task_block(&world, &review),
        Some(("invalid_brief".to_string(), reason))
    );
}

/// A review of task `of` in the world's repository, once that task is done.
fn a_review(world: &World, text: &str, of: &str) -> NewTask {
    NewTask::new(text)
        .label("repo", world.repo_label())
        .label("of", of)
        .needs(of)
        .role("review")
}

/// The options rider starts a fresh review's `codex` with, working in `dir`.
fn codex_options(dir: &std::path::Path) -> Vec<String> {
    let dir = dir.to_str().expect("UTF-8 path");
    let head = ["exec", "--model", REVIEW_MODEL, "--json"];
    let tail = ["--dangerously-bypass-approvals-and-sandbox", "-C", dir];
    head.iter()
        .chain(&tail)
        .map(|arg| arg.to_string())
        .collect()
}

/// The options rider starts a resumed review's `codex` with; the session follows `--`.
fn codex_resume_options() -> Vec<String> {
    let options = ["exec", "resume", "--model", REVIEW_MODEL, "--json"];
    let options = options
        .iter()
        .chain(&["--dangerously-bypass-approvals-and-sandbox"]);
    options.map(|arg| arg.to_string()).collect()
}

/// The session a resumed `codex` of `task` was asked to resume: the first argument after `--`.
fn resumed_session(world: &World, task: &str, attempt: u32) -> Option<String> {
    let args = fake_agent::args(&world.root, task, attempt);
    let mut after = args.lines().skip_while(|line| *line != "--").skip(1);
    after.next().map(str::to_string)
}

fn replies(world: &World, task: &str) -> Vec<String> {
    let replies = task_replies(world, task);
    replies
        .iter()
        .filter_map(|reply| reply["text"].as_str().map(str::to_string))
        .collect()
}
