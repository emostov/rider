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

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "completed"]);
    assert_eq!(exit, Some(0));
    let runs = agent_runs_for(&world, &review);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].harness, "codex-review");
    let args = fake_agent::args(&world.root, &review, 0);
    let expected = r#"review
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

#[test]
fn a_review_stopped_by_the_content_filter_is_rerun_once_on_mythos_with_the_same_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let brief = "Review the work of the sandbox: git diff main...HEAD.";
    let review = NewTask::new(brief)
        .label("repo", world.repo_label())
        .label("of", &built)
        .needs(&built)
        .role("review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::ContentFilterStop]);
    let native = when_the_lead_creates(&world, &a_native_review_of(&world, &built));
    given_the_agent_will(&world, &native, &[Behaviour::ContentFilterStop]);

    let tasks = [built.as_str(), review.as_str(), native.as_str()];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);
    world.vb_lead(&["tasks", "reopen", &review], None);
    let (again, _) = when_the_daemon_runs_until_settled(&mut world, &[&review]);

    assert_eq!(states, vec!["completed", "failed", "failed"]);
    assert_eq!(exit, Some(0));
    assert_eq!(again, vec!["failed"]);
    let reruns = reruns_on_the_board(&world);
    assert_eq!(reruns.len(), 2, "{reruns:?}");
    for (failed, text) in [
        (&review, brief),
        (&native, "Review the work: git diff main...HEAD."),
    ] {
        let rerun = reruns_of(&reruns, failed);
        let read: serde_json::Value = serde_json::from_str(&task_text(&world, &rerun)).unwrap();
        assert_eq!(read["message"]["text"], json!(text));
        assert_eq!(read["message"]["task"]["needs"], json!([built]));
        let labels = task_labels(&world, &rerun);
        let expected = [
            ("of", built.as_str()),
            ("repo", world.repo_label()),
            ("rerun_of", failed.as_str()),
            ("role", "security"),
        ];
        let wanted: Vec<(String, String)> = (expected.iter())
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let got: Vec<(String, String)> = (labels.into_iter())
            .filter(|(key, _)| expected.iter().any(|(wanted, _)| wanted == key))
            .collect();
        assert_eq!(got, wanted);
        assert_eq!(task_label(&world, failed, "rerun"), Some(rerun.clone()));
        let replies = replies_text(&world, failed);
        let note = format!("rerun once on Mythos as security task #{rerun}");
        assert_eq!(replies.matches(&note).count(), 1, "{replies}");
    }
}

fn reruns_on_the_board(world: &World) -> Vec<(String, String)> {
    let all = tasks_in_state(world, "all");
    let labelled = all.into_iter().filter_map(|id| {
        let of = task_label(world, &id, "rerun_of")?;
        Some((id, of))
    });
    labelled.collect()
}

fn reruns_of(reruns: &[(String, String)], failed: &str) -> String {
    let mut found = reruns.iter().filter(|(_, of)| of == failed);
    let (rerun, _) = found
        .next()
        .unwrap_or_else(|| panic!("no rerun of #{failed}"));
    assert_eq!(found.next(), None, "a second rerun of #{failed}");
    rerun.clone()
}

fn replies_text(world: &World, id: &str) -> String {
    let replies = task_replies(world, id);
    let texts: Vec<String> = replies.iter().map(|reply| reply.to_string()).collect();
    texts.join("\n")
}
