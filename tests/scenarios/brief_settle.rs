use std::fs;

use serde_json::Value;

use crate::fake_agent::{self, BLOCK_QUESTION, Behaviour};
use crate::steps::*;
use crate::world::{MAIN, World};

fn task_result(world: &World, id: &str) -> Option<String> {
    let read: Value = serde_json::from_str(&task_text(world, id)).expect("vb read JSON");
    read["message"]["task"]["result"]
        .as_str()
        .map(str::to_string)
}

fn then_the_task_failed_with(world: &World, id: &str, reason: &str) {
    assert_eq!(task_state(world, id), "failed", "{id}");
    let text = task_text(world, id);
    assert!(text.contains(reason), "{id} lacks {reason:?}: {text}");
}

#[test]
fn each_invalid_brief_is_blocked_with_the_label_to_fix_and_a_hash_of_label_is_accepted() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let plain = world.root.join("plain");
    fs::create_dir_all(&plain).expect("create a plain directory");
    let plain = plain.to_str().expect("UTF-8 path").to_string();
    let mut shown: String = plain.chars().take(80).collect();
    if shown.len() < plain.len() {
        shown.push('…');
    }
    let long = "9x".repeat(64);
    let cut = format!("{}…", &long[..80]);
    let implement = |labels: &[(&str, &str)]| {
        let mut task = NewTask::new("Add the work.").role("implement");
        for (key, value) in labels {
            task = task.label(key, value);
        }
        task
    };
    let repo = world.repo_label().to_string();
    let cases: Vec<(NewTask, String)> = vec![
        (
            implement(&[("repo", "src/rider")]),
            "Change the `repo` label from \"src/rider\" to an absolute path.".to_string(),
        ),
        (
            implement(&[("repo", "/nonexistent/rider-brief-repo")]),
            "Change the `repo` label to the path of a git repository: \
             \"/nonexistent/rider-brief-repo\" is not a directory."
                .to_string(),
        ),
        (
            implement(&[("repo", &plain)]),
            format!(
                "Change the `repo` label to the path of a git repository: {shown:?} is not \
                 inside a git repository."
            ),
        ),
        (
            implement(&[("repo", &repo), ("base", "1"), ("onto", "2")]),
            "Remove either the `base` label or the `onto` label, since a task may not have both."
                .to_string(),
        ),
        (
            implement(&[("repo", &repo), ("base", "12a")]),
            "Change the `base` label from \"12a\" to a task id such as #812.".to_string(),
        ),
        (
            implement(&[("repo", &repo), ("onto", &long)]),
            format!("Change the `onto` label from {cut:?} to a task id such as #812."),
        ),
        (
            NewTask::new("Review the work.")
                .label("repo", &repo)
                .label("base", "3")
                .role("review"),
            "Set the `of` label to the id of the task whose work this role checks.".to_string(),
        ),
        (
            NewTask::new("Review the work.")
                .label("repo", &repo)
                .label("of", "x")
                .role("security"),
            "Change the `of` label from \"x\" to a task id such as #812.".to_string(),
        ),
        (
            implement(&[("repo", &repo), ("from", "-main")]),
            "Change the `from` label from \"-main\" to a branch name without spaces or `..` \
             that does not start with `-`."
                .to_string(),
        ),
        (
            implement(&[("repo", &repo), ("from", "a b")]),
            "Change the `from` label from \"a b\" to a branch name without spaces or `..` \
             that does not start with `-`."
                .to_string(),
        ),
        (
            implement(&[("repo", &repo), ("from", "main..x")]),
            "Change the `from` label from \"main..x\" to a branch name without spaces or `..` \
             that does not start with `-`."
                .to_string(),
        ),
        (
            implement(&[("repo", &repo), ("resume", "yes")]),
            "Change the `resume` label from \"yes\" to 1, or remove it.".to_string(),
        ),
        (
            implement(&[("repo", &repo), ("restack", "yes")]),
            "Change the `restack` label from \"yes\" to 1, or remove it.".to_string(),
        ),
        (
            implement(&[("repo", &repo), ("restack", "1")]),
            "Remove the `restack` label, or set `onto` to the task whose branch this task \
             rebases."
                .to_string(),
        ),
    ];
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let qa = NewTask::new("Run the work.")
        .label("repo", &repo)
        .label("of", &format!("#{built}"))
        .label("from", MAIN)
        .needs(&built)
        .role("qa");
    let qa = when_the_lead_creates(&world, &qa);
    given_the_agent_will(&world, &qa, &[Behaviour::CommitNothing]);
    let ids: Vec<String> = cases
        .iter()
        .map(|(task, _)| when_the_lead_creates(&world, task))
        .collect();
    let mut all: Vec<&str> = ids.iter().map(String::as_str).collect();
    all.extend([built.as_str(), qa.as_str()]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &all);

    let mut expected = vec!["blocked".to_string(); ids.len()];
    expected.extend(["completed".to_string(), "completed".to_string()]);
    assert_eq!(
        (states, exit),
        (expected, Some(0)),
        "{}",
        world.daemon_log()
    );
    for (id, (_, reason)) in ids.iter().zip(&cases) {
        let block = Some(("invalid_brief".to_string(), reason.clone()));
        assert_eq!(task_block(&world, id), block, "{id}");
        assert_eq!(agent_runs_for(&world, id), vec![], "{id}");
    }
}

#[test]
fn each_failed_attempt_is_failed_with_the_reason_its_evidence_shows() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let implement = [
        Behaviour::ExitNonZero,
        Behaviour::ExitOnSignal,
        Behaviour::CommitReportingFailure,
        Behaviour::LeaveBranch,
        Behaviour::BlockThenExitNonZero,
    ];
    let qa = [
        Behaviour::LeaveBranch,
        Behaviour::Commit,
        Behaviour::LeaveUncommitted,
    ];
    let mut ids: Vec<String> = Vec::new();
    for behaviour in implement {
        let id = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
        given_the_agent_will(&world, &id, &[behaviour]);
        ids.push(id);
    }
    for behaviour in qa {
        let id = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Run the work."));
        given_the_agent_will(&world, &id, &[behaviour]);
        ids.push(id);
    }
    let mut all: Vec<&str> = vec![&built];
    all.extend(ids.iter().map(String::as_str));

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &all);

    let expected = [
        "completed",
        "failed",
        "failed",
        "failed",
        "failed",
        "blocked",
        "failed",
        "failed",
        "failed",
    ];
    assert_eq!(
        (states, exit),
        (expected.map(String::from).to_vec(), Some(0))
    );
    let branch = task_label(&world, &ids[3], "branch").expect("the task's branch");
    let reasons = [
        format!("agent exited {}", fake_agent::FAIL_EXIT_CODE),
        "agent exited on a signal".to_string(),
        "agent reported failure in its output stream".to_string(),
        format!("no committed work: the worktree is not on its branch {branch}"),
    ];
    for (id, reason) in ids.iter().zip(&reasons) {
        then_the_task_failed_with(&world, id, reason);
    }
    let block = Some(("needs_input".to_string(), BLOCK_QUESTION.to_string()));
    assert_eq!(task_block(&world, &ids[4]), block);
    then_the_task_failed_with(&world, &ids[5], "review changed the checkout");
    then_the_task_failed_with(&world, &ids[6], "review changed the checkout");
    let dirty = "no committed work: the worktree has uncommitted changes";
    then_the_task_failed_with(&world, &ids[7], dirty);
}

#[test]
fn a_completed_attempt_takes_the_agents_last_message_or_says_completed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let silent = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &silent, &[Behaviour::CommitSilently]);
    let blank = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &blank, &[Behaviour::CommitReportingBlank]);
    let qa = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Run the work."));
    given_the_agent_will(&world, &qa, &[Behaviour::CommitNothing]);

    let tasks = [built.as_str(), &silent, &blank, &qa];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!((states, exit), (vec!["completed".to_string(); 4], Some(0)));
    let results: Vec<Option<String>> = tasks.iter().map(|id| task_result(&world, id)).collect();
    let summary = |id: &str| Some(fake_agent::summary(id, 0));
    let completed = Some("completed".to_string());
    assert_eq!(
        results,
        vec![summary(&built), completed.clone(), completed, summary(&qa)]
    );
}
