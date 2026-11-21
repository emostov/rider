use std::fs;
use std::thread;
use std::time::Duration;

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
            implement(&[("repo", &repo), ("base", "##1")]),
            "Change the `base` label from \"##1\" to a task id such as #812.".to_string(),
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
        Behaviour::ComplainThenExitNonZero,
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
    let log = world.home.join("logs").join(&ids[0]).join("1.jsonl");
    let log = std::fs::read_to_string(&log).expect("read the failed attempt's log");
    assert!(log.contains(fake_agent::STDERR_LINE), "{log}");
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

#[test]
fn an_agent_that_outlives_a_shortened_attempt_limit_is_failed_for_its_time_limit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "attempt=2000,tick=200")]);

    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (vec!["failed".to_string()], Some(0))
    );
    then_the_task_failed_with(&world, &task, "the attempt reached its time limit");
}

#[test]
fn an_agent_that_outlives_the_runners_limit_is_failed_as_timed_out() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    let timing = "attempt=30000,runner=2000,tick=200";
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, timing)]);

    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (vec!["failed".to_string()], Some(0))
    );
    let read: Value = serde_json::from_str(&task_text(&world, &task)).expect("vb read JSON");
    let reason = &read["message"]["task"]["reason"];
    assert_eq!(reason, "timed out after 55 minutes");
}

#[test]
fn an_attempt_whose_new_commits_another_branch_also_holds_fails_with_none_of_its_own() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::CommitShared]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    let reason = "no committed work: the branch has no commits of its own";
    then_the_task_failed_with(&world, &task, reason);
}

#[test]
fn a_role_removed_or_changed_after_the_claim_is_blocked_naming_every_role() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let roles = "one of implement, light, review, native-review, security or qa.";
    let long = "x".repeat(128);
    let cut = format!("{}…", "x".repeat(80));
    let cases = [
        (None, format!("Set the `role` label to {roles}")),
        (
            Some("lead"),
            format!("Change the `role` label from \"lead\" to {roles}"),
        ),
        (
            Some(long.as_str()),
            format!("Change the `role` label from {cut:?} to {roles}"),
        ),
    ];
    let ids: Vec<String> = cases
        .iter()
        .map(|_| when_the_lead_creates(&world, &an_implement_task(&world, "Add the work.")))
        .collect();
    let tasks: Vec<&str> = ids.iter().map(String::as_str).collect();
    let release = world.root.join("reads-released");
    given_vb_holds_the_first_read_of(&world, &tasks, &release);
    given_a_running_daemon(&mut world);
    for (id, (role, _)) in ids.iter().zip(&cases) {
        when_the_file_exists(&world, &world.root.join(format!("marker-{id}")));
        match role {
            None => world.vb_lead(&["label", id, "--unset", "role"], None),
            Some(role) => world.vb_lead(&["label", id, &format!("role={role}")], None),
        };
    }
    fs::write(&release, "").expect("release the held reads");

    let states = when_the_tasks_settle(&world, &tasks);
    when_the_daemon_is_sent_sigterm(&world);

    let blocked = vec!["blocked".to_string(); ids.len()];
    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (blocked, Some(0))
    );
    for (id, (_, reason)) in ids.iter().zip(&cases) {
        let block = Some(("invalid_brief".to_string(), reason.clone()));
        assert_eq!(task_block(&world, id), block, "{id}");
    }
    assert_eq!(agent_runs(&world), vec![]);
}

#[test]
fn an_onto_task_whose_branch_has_too_many_tasks_to_check_is_blocked_naming_the_onto_label() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    for target in 9001..9034 {
        let holder = NewTask::new("Hold a branch.")
            .label("repo", world.repo_label())
            .label("onto", &target.to_string());
        when_the_lead_creates(&world, &holder);
    }
    let task = NewTask::new("Fix the work.")
        .label("repo", world.repo_label())
        .label("onto", "9000")
        .role("implement");
    let task = when_the_lead_creates(&world, &task);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let reason = "Finish or cancel unfinished tasks, or remove the `onto` label: 2 of them are \
                  past rider's bounds, so it cannot check which are unfinished on the branch of \
                  task #9000.";
    assert_eq!(
        task_block(&world, &task),
        Some(("invalid_brief".to_string(), reason.to_string()))
    );
    assert_eq!(agent_runs(&world), vec![]);
}

#[test]
fn a_lead_reply_posted_while_the_agent_runs_resumes_its_session_and_changes_the_result() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::ReportDecision]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &task);

    world.vb_lead(
        &["reply", &task, "-"],
        Some("Decision blue: use the blue name."),
    );
    world.release_agent(&task);
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(
        task_result(&world, &task),
        Some(fake_agent::heard(&["blue"]))
    );
    let session = fake_agent::session(&task, 0);
    let mut resumed = fake_agent::options(&world.root, &task, 0);
    resumed.extend(["--resume".to_string(), session]);
    assert_eq!(fake_agent::options(&world.root, &task, 1), resumed);
    let prompt = agent_prompt(&world, &task, 1);
    assert!(
        prompt.contains("Decision blue: use the blue name."),
        "{prompt}"
    );
    let replies = task_replies(&world, &task);
    let noted = replies.iter().filter_map(|reply| reply["text"].as_str());
    let noted: Vec<&str> = noted.filter(|text| text.contains("posted while")).collect();
    assert_eq!(noted.len(), 1, "{replies:?}");
}

#[test]
fn a_reply_from_another_worker_is_not_delivered_to_the_running_agent() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::ReportDecision]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &task);

    let note = world.vb_as(
        "implement-9-other",
        &["reply", &task, "-"],
        Some("Decision red."),
    );
    assert_eq!(note.code, Some(0), "{note:?}");
    world.release_agent(&task);
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(
        task_result(&world, &task),
        Some(fake_agent::summary(&task, 0))
    );
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
}

fn when_the_lead_replies(world: &World, task: &str, text: &str) {
    world.vb_lead(&["reply", task, "-"], Some(text));
}

fn rider_notes(world: &World, task: &str, words: &str) -> usize {
    let replies = task_replies(world, task);
    let texts = replies.iter().filter_map(|reply| reply["text"].as_str());
    texts.filter(|text| text.contains(words)).count()
}

#[test]
fn a_reply_from_a_worker_whose_profile_is_named_like_a_lead_is_not_delivered() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::ReportDecision]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &task);

    let note = world.vb_as(
        "lead-impostor",
        &["reply", &task, "-"],
        Some("Decision red."),
    );
    assert_eq!(note.code, Some(0), "{note:?}");
    world.release_agent(&task);
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(
        task_result(&world, &task),
        Some(fake_agent::summary(&task, 0))
    );
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
}

#[test]
fn a_task_canceled_while_rider_announces_a_reply_delivery_starts_no_second_agent() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::ReportDecision]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &task);
    when_the_lead_replies(&world, &task, "Decision blue.");
    let (marker, release) = (world.root.join("reply-held"), world.root.join("reply-go"));
    given_vb_waits_for(&world, "reply", &marker, &release);

    world.release_agent(&task);
    when_the_file_exists(&world, &marker);
    world.vb_lead(&["tasks", "cancel", &task, "superseded", "--json"], None);
    fs::write(&release, "").expect("release the held reply");
    let states = when_the_tasks_settle(&world, &[&task]);
    thread::sleep(Duration::from_secs(3));
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(states, vec!["canceled".to_string()]);
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
}

#[test]
fn a_reply_is_delivered_though_the_first_thread_read_after_the_agent_fails() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::ReportDecision]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &task);
    when_the_lead_replies(&world, &task, "Decision blue.");
    let armed = world.root.join("fail-next-read");
    given_vb_fails_one_read_once(&world, &armed);
    fs::write(&armed, "").expect("arm the failing read");

    world.release_agent(&task);
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["completed".to_string()]);
    assert!(!armed.exists(), "no read failed");
    assert_eq!(
        task_result(&world, &task),
        Some(fake_agent::heard(&["blue"]))
    );
}

#[test]
fn a_codex_agent_run_again_gets_every_reply_so_far_and_stops_after_three_rounds() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.implement]\nharness = \"codex\"\n");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::HoldAttempt]);
    given_a_running_daemon(&mut world);

    for (attempt, word) in (0..4).zip(["one", "two", "three", "four"]) {
        when_the_agent_attempt_has_started(&world, &task, attempt);
        when_the_lead_replies(&world, &task, &format!("Decision {word}."));
        fake_agent::release_attempt(&world.root, &task, attempt);
    }
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["failed".to_string()]);
    assert_eq!(agent_runs_for(&world, &task).len(), 4);
    let prompt = agent_prompt(&world, &task, 3);
    for decision in ["Decision one.", "Decision two.", "Decision three."] {
        assert!(prompt.contains(decision), "{decision} missing: {prompt}");
    }
    assert!(!prompt.contains("Decision four."), "{prompt}");
    then_the_task_failed_with(&world, &task, "were not delivered to it");
}

#[test]
fn a_reply_delivered_to_a_qa_agent_asks_for_a_report_and_no_commit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let qa = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Check the work."));
    given_the_agent_will(
        &world,
        &qa,
        &[Behaviour::HoldThenNothing, Behaviour::ReportDecision],
    );
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &qa);

    when_the_lead_replies(&world, &qa, "Decision green.");
    world.release_agent(&qa);
    let states = when_the_tasks_settle(&world, &[&built, &qa]);

    assert_eq!(
        states,
        vec!["completed".to_string(); 2],
        "{}",
        task_text(&world, &qa)
    );
    assert_eq!(
        agent_prompt(&world, &qa, 1),
        "These replies were posted in your task's thread while you worked. They amend the \
         brief, and the latest wins. Act on them without committing, and end with a new final \
         report.\n\nFrom lead:\nDecision green.\n"
    );
}

#[test]
fn a_reply_posted_while_rider_completes_the_task_is_noted_as_undelivered() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let (marker, release) = (
        world.root.join("complete-held"),
        world.root.join("complete-go"),
    );
    given_vb_waits_for(&world, "complete", &marker, &release);
    given_a_running_daemon(&mut world);

    when_the_file_exists(&world, &marker);
    when_the_lead_replies(&world, &task, "Decision late.");
    fs::write(&release, "").expect("release the held completion");
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(
        rider_notes(&world, &task, "landed while rider completed"),
        1
    );
}
