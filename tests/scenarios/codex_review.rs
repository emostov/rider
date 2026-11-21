//! A native review runs as `codex review`, its prompt on standard input, settled by its exit
//! status and the final text on its standard output.

use serde_json::json;

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::World;

fn a_native_review_of(world: &World, built: &str) -> NewTask {
    NewTask::new("Review the work: git diff main...HEAD.")
        .label("repo", world.repo_label())
        .label("of", built)
        .needs(built)
        .role("native-review")
}

#[test]
fn a_native_review_runs_codex_review_with_its_prompt_on_stdin_and_completes_with_its_text() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = when_the_lead_creates(&world, &a_native_review_of(&world, &built));
    given_the_agent_will(&world, &review, &[Behaviour::CommitNothing]);
    given_the_users_codex_config_declares_mcp_servers(&world, &["alpha"]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "completed"]);
    assert_eq!(exit, Some(0));
    let runs = agent_runs_for(&world, &review);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].harness, "codex-review");
    let args = fake_agent::args(&world.root, &review, 0);
    let expected = r#"review
-c
mcp_servers.alpha.enabled=false
-c
model="gpt-6.2-sol"
-c
review_model="gpt-6.2-sol"
-c
sandbox_mode="danger-full-access"
-c
approval_policy="never"
-
"#;
    assert_eq!(args, expected);
    let prompt = fake_agent::stdin(&world.root, &review, 0);
    assert!(
        prompt.starts_with("# You review another task's work\n"),
        "{prompt}"
    );
    assert!(prompt.contains("## Simpler and faster\n"), "{prompt}");
    let unit_tests = "A new unit test, or a test that calls internal functions directly";
    assert!(prompt.contains(unit_tests), "{prompt}");
    assert!(
        prompt.contains("Review the work: git diff main...HEAD."),
        "{prompt}"
    );
    let records = task_records(&world, &review, "rider-result");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["summary"],
        json!(fake_agent::summary(&review, 0))
    );
    assert_eq!(records[0]["commits"], json!(0));
}

#[test]
fn a_native_review_whose_codex_review_exits_non_zero_fails() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = when_the_lead_creates(&world, &a_native_review_of(&world, &built));
    given_the_agent_will(&world, &review, &[Behaviour::ExitNonZero]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "failed"]);
    assert_eq!(exit, Some(0));
    assert_eq!(agent_runs_for(&world, &review)[0].harness, "codex-review");
    assert_eq!(
        task_records(&world, &review, "rider-result"),
        Vec::<serde_json::Value>::new()
    );
}

#[test]
fn a_native_review_that_exits_0_without_a_review_fails() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let silent = when_the_lead_creates(&world, &a_native_review_of(&world, &built));
    given_the_agent_will(&world, &silent, &[Behaviour::ReviewNothing]);
    let interrupted = when_the_lead_creates(&world, &a_native_review_of(&world, &built));
    given_the_agent_will(&world, &interrupted, &[Behaviour::ReviewInterrupted]);

    let tasks = [built.as_str(), silent.as_str(), interrupted.as_str()];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed", "failed", "failed"]);
    assert_eq!(exit, Some(0));
    for review in [&silent, &interrupted] {
        assert_eq!(
            task_records(&world, review, "rider-result"),
            Vec::<serde_json::Value>::new()
        );
    }
    let replies = replies_text(&world, &interrupted);
    assert!(
        replies.contains(fake_agent::REVIEW_INTERRUPTED),
        "{replies}"
    );
}

#[test]
fn a_native_review_stopped_by_the_content_filter_fails_showing_codexs_error() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = when_the_lead_creates(&world, &a_native_review_of(&world, &built));
    given_the_agent_will(&world, &review, &[Behaviour::ContentFilterStop]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "failed"]);
    assert_eq!(exit, Some(0));
    let replies = replies_text(&world, &review);
    assert!(
        replies.contains(fake_agent::CONTENT_FILTER_ERROR),
        "{replies}"
    );
}

fn replies_text(world: &World, id: &str) -> String {
    let replies = task_replies(world, id);
    let texts: Vec<String> = replies.iter().map(|reply| reply.to_string()).collect();
    texts.join("\n")
}
