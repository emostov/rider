//! Black-box scenarios: the real `rider daemon` against a real board, a scratch repository, and
//! fake agents, each written as Given / When / Then.

#[expect(
    dead_code,
    reason = "behaviours and readers for the scenarios still to come"
)]
mod fake_agent;
#[expect(dead_code, reason = "steps for the scenarios still to come")]
mod steps;
mod world;

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use fake_agent::{Behaviour, Run};
use steps::*;
use world::{MAIN, PROJECT, World};

#[test]
fn an_implement_task_completes_with_its_commit_and_a_result_record() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));
    given_the_agent_will(&world, &task, &[Behaviour::Commit]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

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
        "behind": [],
    });
    assert_eq!(task_records(&world, &task, "rider-result"), vec![record]);
}

#[test]
fn a_fix_that_moves_a_branch_names_the_stacked_task_left_behind_in_its_result_record() {
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
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the base work.", &base, &stacked, false),
    );

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&base, &stacked, &fix]);

    assert_eq!(states, vec!["completed", "completed", "completed"]);
    assert_eq!(exit, Some(0));
    let branch = task_label(&world, &base, "branch").expect("the base task's branch");
    assert_eq!(task_label(&world, &fix, "branch"), Some(branch.clone()));
    let stacked_id: u64 = stacked.parse().expect("a task id");
    let record = json!({
        "branch": branch,
        "head": branch_head(&world, &branch),
        "commits": 1,
        "summary": fake_agent::summary(&fix, 0),
        "behind": [stacked_id],
    });
    assert_eq!(task_records(&world, &fix, "rider-result"), vec![record]);
}

#[test]
fn a_second_fix_onto_an_earlier_fix_names_the_task_stacked_on_the_first_one() {
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
    let first = a_fix(&world, "Fix the base work.", &base, &stacked, false);
    let first = when_the_lead_creates(&world, &first);
    let second = a_fix(&world, "Fix the first fix.", &first, &first, false);
    let second = when_the_lead_creates(&world, &second);

    let tasks = [
        base.as_str(),
        stacked.as_str(),
        first.as_str(),
        second.as_str(),
    ];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed"; 4]);
    assert_eq!(exit, Some(0));
    let branch = task_label(&world, &base, "branch").expect("the base task's branch");
    assert_eq!(task_label(&world, &second, "branch"), Some(branch.clone()));
    let stacked_id: u64 = stacked.parse().expect("a task id");
    let record = json!({
        "branch": branch,
        "head": branch_head(&world, &branch),
        "commits": 1,
        "summary": fake_agent::summary(&second, 0),
        "behind": [stacked_id],
    });
    assert_eq!(task_records(&world, &second, "rider-result"), vec![record]);
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

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&base, &stacked]);

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

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &qa]);

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

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &qa]);

    assert_eq!(states, vec!["completed".to_string(), "failed".to_string()]);
    assert_eq!(exit, Some(0));
    assert_eq!(agent_runs_for(&world, &qa).len(), 1);
    let branch = then_the_branch_has_the_agents_commit(&world, &built, 0);
    assert_eq!(branches(&world), vec![MAIN.to_string(), branch]);
}

#[test]
fn a_review_runs_detached_at_the_reviewed_head_and_completes_leaving_the_branch_alone() {
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
    given_the_agent_will(&world, &review, &[Behaviour::RecordHead]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "completed"]);
    assert_eq!(exit, Some(0));
    let branch = then_the_branch_has_the_agents_commit(&world, &built, 0);
    let head = branch_head(&world, &branch);
    assert_eq!(fake_agent::head(&world.root, &review, 0), (head, None));
    let runs = agent_runs_for(&world, &review);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].dir, worktree_dir(&world, &review));
    assert_eq!(task_label(&world, &review, "branch"), None);
    assert_eq!(branches(&world), vec![MAIN.to_string(), branch]);
    then_the_worktree_is_removed(&world, &review);
    then_only_the_repository_is_a_worktree(&world);
}

#[test]
fn a_task_with_no_repo_label_is_blocked_as_an_invalid_brief_and_no_agent_runs() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = NewTask::new("Add the work.")
        .label("from", MAIN)
        .role("implement");
    let task = when_the_lead_creates(&world, &task);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let reason = "Set the `repo` label to the absolute path of the repository.";
    assert_eq!(
        task_block(&world, &task),
        Some(("invalid_brief".to_string(), reason.to_string()))
    );
    assert_eq!(agent_runs(&world), vec![]);
    assert_eq!(branches(&world), vec![MAIN.to_string()]);
}

#[test]
fn an_agent_that_commits_nothing_fails_and_once_reopened_completes_on_the_same_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(
        &world,
        &task,
        &[Behaviour::CommitNothing, Behaviour::Commit],
    );
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["failed"]);
    let text = task_text(&world, &task);
    let reason = "no committed work: the branch has no commits of its own";
    assert!(text.contains(reason), "{text}");
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let first = task_label(&world, &task, "branch").expect("the failed task's branch");

    world.vb_lead(&["tasks", "reopen", &task], None);
    let states = when_the_tasks_settle(&world, &[&task]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(
        (states, then_the_daemon_exits(&mut world)),
        (vec!["completed".to_string()], Some(0))
    );
    assert_eq!(
        then_the_branch_has_the_agents_commit(&world, &task, 1),
        first
    );
    assert_eq!(branches(&world), vec![MAIN.to_string(), first]);
    let runs = agent_runs_for(&world, &task);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].dir, runs[1].dir);
    then_the_worktree_is_removed(&world, &task);
    then_only_the_repository_is_a_worktree(&world);
}

#[test]
fn an_agent_that_leaves_uncommitted_changes_fails_and_its_worktree_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::LeaveUncommitted]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["failed".to_string()], Some(0)));
    let text = task_text(&world, &task);
    let reason = "no committed work: the worktree has uncommitted changes";
    assert!(text.contains(reason), "{text}");
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let left = worktree_dir(&world, &task).join(format!("uncommitted-{task}.txt"));
    let left = fs::read_to_string(&left).expect("the uncommitted file is kept");
    assert_eq!(left, "left behind\n");
}

#[test]
fn the_daemon_claims_a_new_task_with_a_role_within_five_seconds() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);

    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));

    assert_ne!(when_the_task_leaves_ready_within(&world, &task, 5), "ready");
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    let log = fs::read_to_string(world.home.join("daemon.log")).expect("read daemon.log");
    for word in ["claimed", "spawned", "settled"] {
        assert!(log.contains(&format!("{word} #{task}")), "{word} in {log}");
    }
}

#[test]
fn the_daemon_claims_a_ready_task_within_five_seconds_of_its_role_being_set() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut new = an_implement_task(&world, "Add one work file.");
    new.role = None;
    let task = when_the_lead_creates(&world, &new);
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_task_leaves_ready_within(&world, &task, 2), "ready");

    world.vb_lead(&["label", &task, "role=implement"], None);

    assert_ne!(when_the_task_leaves_ready_within(&world, &task, 5), "ready");
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_board_whose_identity_cannot_be_read_stops_the_daemon_before_any_claim() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));
    given_vb_fails_the_identity_read(&world);

    let started = when_rider_runs(&world, &["daemon"]);

    assert_eq!(started.code, Some(1), "{started:?}");
    assert!(started.stderr.contains(&world.url), "{started:?}");
    assert!(
        started.stderr.contains("identity unavailable"),
        "{started:?}"
    );
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_block(&world, &task), None);
    assert_eq!(agent_runs(&world), vec![]);
}

#[test]
fn a_second_daemon_on_the_same_home_exits_naming_the_holder() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let holder = world
        .daemon
        .as_ref()
        .expect("the daemon")
        .child
        .id()
        .to_string();

    let second = when_rider_runs(&world, &["daemon"]);

    assert_eq!(second.code, Some(1), "{second:?}");
    assert!(
        second.stderr.contains(&format!("pid {holder}")),
        "{second:?}"
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn sigterm_drains_the_running_agent_claims_nothing_new_and_exits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let running = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &running, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &running);

    when_the_daemon_is_sent_sigterm(&world);
    let later = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
    assert_eq!(
        when_the_task_leaves_ready_within(&world, &later, 3),
        "ready"
    );
    world.release_agent(&running);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &running), "completed");
    assert_eq!(task_state(&world, &later), "ready");
    assert!(agent_runs_for(&world, &later).is_empty());
}

#[test]
fn a_daemon_drained_by_sigterm_leaves_no_vb_tail_behind() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    assert_eq!(daemon_feeds(&world).len(), 1, "{:?}", daemon_feeds(&world));

    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(daemon_feeds(&world), Vec::<String>::new());
}

#[test]
fn a_second_sigterm_stops_the_agent_and_leaves_its_task_ready_with_attempt_unchanged() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    let run = when_the_agent_has_started(&world, &task);

    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_a_second_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(!is_alive(run.pid), "the agent {} still runs", run.pid);
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_label(&world, &task, "attempt"), None);
}

#[test]
fn a_second_sigterm_kills_an_agent_that_ignores_sigterm_within_the_grace_and_releases_its_task() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::HoldIgnoringTerm]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "grace=1000,tick=200")]);
    let run = when_the_agent_has_started(&world, &task);

    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_a_second_sigterm(&world);

    assert!(
        when_the_process_ends_within(run.pid, 5),
        "the agent {} still runs:\n{}",
        run.pid,
        world.daemon_log()
    );
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_label(&world, &task, "attempt"), None);
}

#[test]
fn five_ready_polish_tasks_never_run_more_than_the_four_polish_slots_at_once() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tasks: Vec<String> = [
        "Polish the first work.",
        "Polish the second work.",
        "Polish the third work.",
        "Polish the fourth work.",
        "Polish the fifth work.",
    ]
    .iter()
    .map(|text| {
        let new = NewTask::new(text)
            .label("repo", world.repo_label())
            .label("from", MAIN)
            .role("polish");
        let task = when_the_lead_creates(&world, &new);
        given_the_agent_will(&world, &task, &[Behaviour::Hold]);
        task
    })
    .collect();
    given_a_rider_toml(&world, "[roles.polish]\nslots = 4\n");
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "tick=200")]);
    when_the_agent_has_started(&world, &tasks[0]);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(agents_running(&world), 4);

    for task in &tasks {
        world.release_agent(task);
    }

    let tasks: Vec<&str> = tasks.iter().map(String::as_str).collect();
    assert_eq!(when_the_tasks_settle(&world, &tasks), vec!["completed"; 5]);
    assert_eq!(world.most_agents_at_once(), 4);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn one_builder_at_once_runs_implement_tasks_one_by_one_while_a_review_runs_beside_them() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nbuilders_at_once = 1\n");
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let first = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
    let second = when_the_lead_creates(&world, &an_implement_task(&world, "Add other work."));
    let review = NewTask::new("Review the new work.")
        .label("repo", world.repo_label())
        .label("of", &built)
        .needs(&built)
        .role("review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &first, &[Behaviour::Hold]);
    given_the_agent_will(&world, &second, &[Behaviour::Hold]);
    given_the_agent_will(&world, &review, &[Behaviour::Hold]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "tick=200")]);

    when_the_agent_has_started(&world, &first);
    when_the_agent_has_started(&world, &review);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(agents_running(&world), 2);
    assert_eq!(agent_runs_for(&world, &second).len(), 0);
    world.release_agent(&first);
    when_the_agent_has_started(&world, &second);
    assert_eq!(agents_running(&world), 2);
    world.release_agent(&second);
    world.release_agent(&review);

    let states = when_the_tasks_settle(&world, &[&built, &first, &second]);
    assert_eq!(states, vec!["completed"; 3]);
    assert_eq!(world.most_agents_at_once(), 2);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_rider_toml_giving_review_another_model_starts_the_review_agent_with_it() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.review]\nmodel = \"gpt-test-review\"\n");
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = NewTask::new("Review the new work.")
        .label("repo", world.repo_label())
        .label("of", &built)
        .needs(&built)
        .role("review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::CommitNothing]);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&built, &review]);

    assert_eq!(states, vec!["completed", "completed"]);
    let runs = agent_runs_for(&world, &review);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].harness, "codex");
    assert_eq!(agent_model(&world, &review, 0), "gpt-test-review");
    assert_eq!(agent_model(&world, &built, 0), "claude-opus-5-5");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_rider_toml_adding_a_role_runs_its_task_with_the_roles_own_prompt() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let prompt = world.root.join("docs-prompt.md");
    fs::write(&prompt, "You write the documentation for one task.\n").expect("write a prompt");
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
    let mut new = an_implement_task(&world, "Document the work.");
    new.role = Some("docs".to_string());
    let task = when_the_lead_creates(&world, &new);
    given_a_running_daemon(&mut world);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);

    assert_eq!(agent_model(&world, &task, 0), "claude-docs-model");
    let sent = agent_prompt(&world, &task, 0);
    assert!(
        sent.starts_with("You write the documentation for one task.\n\n# Task "),
        "{sent}"
    );
    then_the_branch_has_the_agents_commit(&world, &task, 0);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_rider_toml_allowing_one_attempt_blocks_a_task_that_failed_once_without_running_it() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nattempts = 1\n");
    let failed_once = an_implement_task(&world, "Add the work.").label("attempt", "1");
    let task = when_the_lead_creates(&world, &failed_once);
    given_a_running_daemon(&mut world);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["blocked"]);

    assert_eq!(agent_runs_for(&world, &task).len(), 0);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn an_unknown_field_in_rider_toml_stops_the_daemon_naming_the_field() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.review]\nmodle = \"gpt-test-review\"\n");

    let output = when_rider_runs(&world, &["daemon"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("modle"), "{output:?}");
    assert!(output.stderr.contains("rider.toml"), "{output:?}");
    assert!(!world.home.join("daemon.lock").exists(), "{output:?}");
}

#[test]
fn a_user_prompt_at_a_built_in_prompts_path_is_refused_naming_the_path() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let prompts = world.home.join("prompts");
    fs::create_dir_all(&prompts).expect("create the prompts directory");
    fs::write(prompts.join("review.md"), "My own review prompt.\n").expect("write a prompt");
    given_a_rider_toml(&world, "[roles.review]\nprompt = \"prompts/review.md\"\n");

    let output = when_rider_runs(&world, &["daemon"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("roles.review.prompt"), "{output:?}");
    assert!(output.stderr.contains("prompts/review.md"), "{output:?}");
    let kept = fs::read_to_string(prompts.join("review.md")).expect("read the prompt");
    assert_eq!(kept, "My own review prompt.\n");
}

#[test]
fn a_prompt_file_named_directly_under_the_rider_home_is_accepted() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    fs::write(world.home.join("docs.md"), "You write docs.\n").expect("write a prompt");
    given_a_rider_toml(&world, "[roles.light]\nprompt = \"docs.md\"\n");

    let output = when_rider_runs(&world, &["config"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert!(output.stdout.contains("prompt = \"docs.md\""), "{output:?}");
}

#[test]
fn the_tact_harness_is_refused_naming_the_harness() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.light]\nharness = \"tact\"\n");

    let output = when_rider_runs(&world, &["config"]);

    assert_ne!(output.code, Some(0), "{output:?}");
    assert!(output.stderr.contains("tact"), "{output:?}");
    assert!(output.stderr.contains("claude"), "{output:?}");
    assert!(output.stderr.contains("codex"), "{output:?}");
}

#[test]
fn a_model_name_with_a_newline_is_refused_naming_the_field() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.light]\nmodel_name = \"Sonnet\\n5.5\"\n");

    let output = when_rider_runs(&world, &["config"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(
        output.stderr.contains("roles.light.model_name"),
        "{output:?}"
    );
}

#[test]
fn a_model_name_longer_than_its_limit_is_refused_naming_the_field() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let long = "M".repeat(200);
    given_a_rider_toml(&world, &format!("[roles.light]\nmodel_name = \"{long}\"\n"));

    let output = when_rider_runs(&world, &["config"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(
        output.stderr.contains("roles.light.model_name"),
        "{output:?}"
    );
}

#[test]
fn a_model_with_a_control_character_is_refused_naming_the_field() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.review]\nmodel = \"gpt\\u0000sol\"\n");

    let output = when_rider_runs(&world, &["config"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("roles.review.model"), "{output:?}");
}

#[test]
fn rider_config_without_a_file_prints_the_built_in_table() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };

    let output = when_rider_runs(&world, &["config"]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let expected = include_str!("rider-config-default.toml");
    assert_eq!(output.stdout, expected);
}

#[test]
fn an_agent_running_past_a_third_of_a_shortened_lease_sees_the_claim_renewed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "lease=15000,tick=200")]);
    when_the_agent_has_started(&world, &task);
    let first = task_claim_until(&world, &task).expect("the task is claimed");

    let later = when_the_claim_moves_past(&world, &task, &first);

    assert!(later > first, "until moved from {first} to {later}");
    world.release_agent(&task);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_shortened_attempt_limit_stops_a_never_exiting_agent_and_fails_its_task() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "attempt=2000,tick=200")]);
    let run = when_the_agent_has_started(&world, &task);

    let started = Instant::now();
    let states = when_the_tasks_settle(&world, &[&task]);

    assert_eq!(states, vec!["failed"]);
    let waited = started.elapsed();
    assert!(waited < Duration::from_secs(15), "failed after {waited:?}");
    assert!(!is_alive(run.pid), "the agent {} still runs", run.pid);
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
}

#[test]
fn a_running_agents_task_canceled_on_the_board_stops_the_agent_and_the_daemon_writes_nothing() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "lease=3000,tick=200")]);
    let run = when_the_agent_has_started(&world, &task);

    world.vb_lead(&["tasks", "cancel", &task, "superseded", "--json"], None);
    let (replies, labels) = (task_replies(&world, &task), task_labels(&world, &task));

    assert!(
        when_the_process_ends_within(run.pid, 3),
        "the agent {} still runs:\n{}",
        run.pid,
        world.daemon_log()
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(task_state(&world, &task), "canceled");
    assert_eq!(task_replies(&world, &task), replies);
    assert_eq!(task_labels(&world, &task), labels);
    assert!(worktree_dir(&world, &task).is_dir(), "the worktree is kept");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_second_sigterm_while_the_fenced_fail_is_in_flight_finishes_the_failure() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::ExitNonZero]);
    let marker = world.root.join("fail-started");
    given_vb_runs_slowly(&world, "fail", &marker, 3);
    given_a_running_daemon(&mut world);
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "no fail began:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(100));
    }

    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_a_second_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &task), "failed");
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    let log = fs::read_to_string(world.home.join("daemon.log")).expect("read daemon.log");
    assert!(
        log.contains("after stop-now: it was written before"),
        "{log}"
    );
}

#[test]
fn a_task_onto_one_that_is_still_settling_waits_for_its_worktree_and_completes() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, false),
    );
    given_the_agent_will(&world, &built, &[Behaviour::Commit]);
    given_the_agent_will(&world, &fix, &[Behaviour::Commit]);
    // The result record is posted after the fenced complete and before the worktree is
    // released, so the fix is ready while the branch is still checked out. Only the result
    // post is held: the daemon posts the board's identity before it follows.
    let (posted, release) = (world.root.join("posted"), world.root.join("release-post"));
    given_vb_waits_for(&world, "rider-result@2", &posted, &release);
    given_a_running_daemon(&mut world);
    when_the_file_exists(&world, &posted);
    when_the_task_is(&world, &fix, "ready");
    fs::write(&release, "").expect("release the held post");

    let states = when_the_tasks_settle(&world, &[&built, &fix]);

    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(states, vec!["completed", "completed"]);
    assert_eq!(agent_runs_for(&world, &fix).len(), 1);
    assert_eq!(
        task_label(&world, &fix, "branch"),
        task_label(&world, &built, "branch")
    );
}

#[test]
fn a_complete_that_fails_twice_is_retried_and_the_task_completes_with_one_agent_run() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Commit]);
    let completes = world.root.join("completes");
    given_vb_fails_complete(&world, 2, &completes);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&task]);

    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(states, vec!["completed"]);
    let tries = fs::read_to_string(&completes).expect("read the complete log");
    assert_eq!(tries.lines().count(), 3, "{tries}");
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
    assert_eq!(task_records(&world, &task, "rider-result").len(), 1);
    then_the_worktree_is_removed(&world, &task);
}

#[test]
fn a_vb_that_hangs_on_claim_marks_the_board_down_after_the_call_limit_and_holds_no_slot() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let marker = world.root.join("claim-started");
    given_vb_runs_slowly(&world, "claim", &marker, 60);
    given_a_running_daemon(&mut world);

    let down = when_rider_status_shows_within(&world, "board down, 0 of", 60);

    assert!(
        down,
        "rider status never said the board was down:\n{}",
        world.daemon_log()
    );
    assert!(marker.exists(), "no claim began");
    assert_eq!(agents_running(&world), 0);
    assert!(boards_the_daemon_started(&world).is_empty());
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_board_the_daemon_started_is_restarted_when_killed_and_its_running_task_completes() {
    let Some(mut world) = World::new() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let first = when_the_daemon_has_started_boards(&world, 1, 5)[0];
    given_the_schemas_and_a_repository(&world);
    let task = a_task_given_its_role_after(&world, "Add the work.", Behaviour::Hold);
    let run = when_the_agent_has_started(&world, &task);

    when_the_group_is_killed(first);
    let boards = when_the_daemon_has_started_boards(&world, 2, 30);
    world.release_agent(&task);
    let states = when_the_tasks_settle(&world, &[&task]);
    let next = a_task_given_its_role_after(&world, "Add more work.", Behaviour::Commit);
    let later = when_the_tasks_settle(&world, &[&next]);

    assert_eq!(states, vec!["completed"]);
    assert_eq!(later, vec!["completed"]);
    assert_eq!(
        agent_runs_for(&world, &task),
        vec![Run { ended: true, ..run }]
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(
        !is_alive(boards[1]),
        "the board the daemon started outlived it"
    );
}

#[test]
fn a_board_left_by_a_killed_daemon_is_kept_by_the_next_and_stopped_by_rider_down() {
    let Some(mut world) = World::new() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let board = when_the_daemon_has_started_boards(&world, 1, 5)[0];
    when_the_daemon_crashes(&mut world);
    assert!(is_alive(board), "the board ended with the daemon");

    given_a_running_daemon(&mut world);
    when_the_daemon_log_shows(&world, &format!("{BOARD_KEPT} {board}"));
    let down = when_rider_runs(&world, &["down"]);

    assert_eq!(down.code, Some(0), "{down:?}");
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(
        when_the_process_ends_within(board, 15),
        "the board {board} outlived the daemon that kept it"
    );
    assert_eq!(boards_the_daemon_started(&world), vec![board]);
}

#[test]
fn a_daemon_log_that_is_a_fifo_is_refused_and_the_daemon_exits() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let log = world.home.join("daemon.log");
    fs::create_dir_all(&world.home).expect("create the rider home");
    let made = std::process::Command::new("/usr/bin/mkfifo")
        .arg(&log)
        .stdin(std::process::Stdio::null())
        .status()
        .expect("run mkfifo");
    assert!(made.success());

    let output = when_rider_runs(&world, &["daemon"]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(
        output.stderr.contains("daemon.log is not a regular file"),
        "{output:?}"
    );
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
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

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
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

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

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &fix]);

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
fn the_daemon_gives_a_refused_resumes_fresh_start_only_what_remains_of_the_attempt_limit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let attempt_minutes = 1;
    let limits = format!("[limits]\nattempt_minutes = {attempt_minutes}\n");
    given_a_rider_toml(&world, &limits);
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, true),
    );
    given_the_agent_will(&world, &fix, &[Behaviour::RefuseResumeSlowly]);

    given_a_running_daemon(&mut world);
    let built_states = when_the_tasks_settle(&world, &[&built]);
    let fix_states =
        when_the_tasks_settle_within(&world, &[&fix], attempt_minutes * 60 + SETTLE_SECONDS_MAX);
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!(built_states, vec!["completed"], "{}", world.daemon_log());
    assert_eq!(fix_states, vec!["failed"], "{}", world.daemon_log());
    assert_eq!(exit, Some(0));
    assert_eq!(agent_runs_for(&world, &fix).len(), 2);
    assert_eq!(task_label(&world, &fix, "attempt").as_deref(), Some("1"));
}

#[test]
fn the_daemon_resumes_a_fix_tasks_session_and_falls_back_to_a_fresh_start_when_refused() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let resumed = a_fix(&world, "Fix the work.", &built, &built, true);
    let resumed = when_the_lead_creates(&world, &resumed);
    let refused = a_fix(&world, "Fix the work again.", &built, &resumed, true);
    let refused = when_the_lead_creates(&world, &refused);
    given_the_agent_will(&world, &refused, &[Behaviour::RefuseResume]);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&built, &resumed, &refused]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(states, vec!["completed".to_string(); 3]);
    let built_session = fake_agent::session(&built, 0);
    let expected = [
        (&resumed, 0, claude_options(Some(&built_session))),
        (&refused, 0, claude_options(Some(&built_session))),
        (&refused, 1, claude_options(None)),
    ];
    for (task, run, options) in expected {
        assert_eq!(
            fake_agent::options(&world.root, task, run),
            options,
            "task {task} run {run}"
        );
    }
    assert_eq!(task_label(&world, &refused, "attempt"), None);
    then_the_branch_has_the_agents_commit(&world, &refused, 1);
}

#[test]
fn a_session_recorded_for_another_repository_is_not_resumed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_a_running_daemon(&mut world);
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
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!(states, vec!["completed".to_string()]);
    assert_eq!(exit, Some(0));
    assert_eq!(
        fake_agent::options(&world.root, &fix, 0),
        claude_options(None)
    );
    let run = &agent_runs_for(&world, &fix)[0];
    assert!(run.dir.ends_with(format!("other-repo/{fix}")), "{run:?}");
}

#[test]
fn a_resumed_fix_of_a_codex_implement_role_starts_fresh() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(
        &world,
        "[roles.implement]\nharness = \"codex\"\nmodel = \"gpt-test-implement\"\n",
    );
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, true),
    );

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &fix]);

    assert_eq!(states, vec!["completed".to_string(); 2]);
    assert_eq!(exit, Some(0));
    let runs = agent_runs_for(&world, &fix);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].harness, "codex");
    assert_eq!(agent_model(&world, &fix, 0), "gpt-test-implement");
}

/// An implement task whose agent will `behave`, given its role only then, so a running
/// daemon cannot claim it before.
fn a_task_given_its_role_after(world: &World, text: &str, behave: Behaviour) -> String {
    let mut new = an_implement_task(world, text);
    new.role = None;
    let task = when_the_lead_creates(world, &new);
    given_the_agent_will(world, &task, &[behave]);
    world.vb_lead(&["label", &task, "role=implement"], None);
    task
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

#[test]
fn reap_lists_and_stops_only_an_orphaned_vb_whose_board_is_gone() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let live = given_another_board(&world, "live-board");
    let mut gone = given_another_board(&world, "gone-board");
    let mut orphans = Orphans::default();
    let own = world.url.clone();
    let on_world = given_an_orphaned_vb_tail(&world, &mut orphans, &own);
    let on_live = given_an_orphaned_vb_tail(&world, &mut orphans, &live.url);
    let on_gone = given_an_orphaned_vb_tail(&world, &mut orphans, &gone.url);
    gone.stop();

    let ours = [
        "reap", "--url", &own, "--url", &live.url, "--url", &gone.url,
    ];
    let listed = when_rider_runs(&world, &ours);

    assert_eq!(listed.code, Some(0), "{listed:?}");
    let shown = format!(
        "pid {on_gone} age <age> vb tail --url {} --as rider-daemon",
        gone.url
    );
    assert_eq!(line_for_pid(&listed.stdout, on_gone), Some(shown));
    assert_eq!(line_for_pid(&listed.stdout, on_live), None);
    assert_eq!(line_for_pid(&listed.stdout, on_world), None);
    assert!(
        listed
            .stdout
            .contains("leaked vb processes; rider reap --yes stops them")
    );
    assert!(is_alive(on_gone), "listing stops nothing");

    given_a_running_daemon(&mut world);
    let logged = format!("leaked pid {on_gone} age ");
    let deadline = Instant::now() + Duration::from_secs(5);
    let log = || fs::read_to_string(world.home.join("daemon.log")).unwrap_or_default();
    while !log().contains(&logged) {
        assert!(
            Instant::now() < deadline,
            "the daemon logged no leak:\n{}",
            log()
        );
        thread::sleep(Duration::from_millis(100));
    }
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(is_alive(on_gone), "the daemon stops nothing");

    let stopped = when_rider_runs(&world, &[&ours[..], &["--yes"]].concat());

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    let line = format!("stopped pid {on_gone} with SIGTERM");
    assert!(stopped.stdout.lines().any(|l| l == line), "{stopped:?}");
    assert!(when_the_process_ends_within(on_gone, 5));
    assert!(is_alive(on_live) && is_alive(on_world));
    let board = world.board.as_mut().expect("the world's board runs");
    assert_eq!(board.try_wait().expect("poll the board"), None);
}

#[test]
fn reap_spares_a_vb_whose_last_url_is_the_configured_board() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dead = an_unused_url();
    let mut orphans = Orphans::default();
    let args = [
        "tail",
        "--url",
        &dead,
        "--url",
        &world.url,
        "--as",
        "rider-daemon",
    ];
    let pid = given_an_orphaned_vb(&world, &mut orphans, &args);

    let listed = when_rider_runs(&world, &["reap", "--url", &dead, "--url", &world.url]);

    assert_eq!(listed.code, Some(0), "{listed:?}");
    assert_eq!(line_for_pid(&listed.stdout, pid), None, "{listed:?}");
    assert!(is_alive(pid));
}

#[test]
fn reap_never_selects_a_board_server() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let dead = an_unused_url();
    let served = an_unused_url();
    let port = served
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches("/api/board");
    let dir = world.root.join("orphaned-serve");
    let dir = dir.to_str().expect("the world's root is UTF-8");
    let mut orphans = Orphans::default();
    let args = [
        "serve",
        "--port",
        port,
        "--no-open",
        "--no-inspect",
        "--dir",
        dir,
        "--url",
        &dead,
    ];
    let pid = given_an_orphaned_vb(&world, &mut orphans, &args);

    let listed = when_rider_runs(&world, &["reap", "--url", &dead]);

    assert_eq!(listed.code, Some(0), "{listed:?}");
    assert_eq!(line_for_pid(&listed.stdout, pid), None, "{listed:?}");
}

#[test]
fn reap_does_not_resolve_a_hostname_and_spares_its_vb() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let named = "http://rider-reap-scenario.invalid:1/api/board";
    let mut orphans = Orphans::default();
    let pid = given_an_orphaned_vb_tail(&world, &mut orphans, named);

    let listed = when_rider_runs(&world, &["reap", "--url", named]);

    assert_eq!(listed.code, Some(0), "{listed:?}");
    assert_eq!(line_for_pid(&listed.stdout, pid), None, "{listed:?}");
}

#[test]
fn a_panicking_world_kills_its_wrapped_vb_and_spares_a_process_that_names_its_path() {
    let Some(mut world) = World::new() else {
        return;
    };
    world.start_board();
    given_vb_runs_slowly(&world, "reply", &world.root.join("replied"), 1);
    let mut observer = Command::new("/usr/bin/tail")
        .arg("-f")
        .arg(world.root.join("board.log"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("start an observer of the world's board log");
    let mut wrapped: Option<Child> = None;

    let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
        let world = world;
        let child = world
            .command(&world.vb)
            .args(["tail", "--url", &world.url, "--as", world::LEAD])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("start the wrapped vb tail");
        let child = wrapped.insert(child);
        thread::sleep(Duration::from_secs(1));
        assert_eq!(
            child.try_wait().expect("poll the wrapped vb").map(|_| ()),
            None
        );
        panic!("the scenario fails while its wrapped vb runs");
    }));

    assert!(unwound.is_err());
    let survived = observer.try_wait().expect("poll the observer").map(|_| ());
    let _ = observer.kill();
    let _ = observer.wait();
    assert_eq!(survived, None, "the observer was killed with the world");
    let mut wrapped = wrapped.expect("the wrapped vb was started");
    let deadline = Instant::now() + Duration::from_secs(5);
    while wrapped.try_wait().expect("poll the wrapped vb").is_none() {
        assert!(
            Instant::now() < deadline,
            "the wrapped vb outlived its world"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Leaves a blocked task with a worktree, replaces the board with `replace`, then runs a new
/// task that reuses the id, and checks it is blocked and the old worktree untouched.
fn then_a_reused_id_on_a_replaced_board_is_blocked(
    mut world: World,
    replace: impl FnOnce(&mut World),
) {
    let old = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &old, &[Behaviour::Block, Behaviour::Commit]);
    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &[&old]);
    assert_eq!(states, vec!["blocked"]);
    let dir = worktree_dir(&world, &old);
    let before = worktree_state(&world, &dir);
    replace(&mut world);

    let new = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&new]);

    assert_eq!(new, old, "the fresh board reuses the id");
    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let text = task_text(&world, &new);
    assert!(text.contains("rider_error"), "{text}");
    assert!(text.contains(dir.to_str().expect("UTF-8 path")), "{text}");
    assert_eq!(agent_runs_for(&world, &new).len(), 1);
    assert_eq!(worktree_state(&world, &dir), before);
}

#[test]
fn a_task_reusing_the_id_of_a_task_on_a_wiped_board_is_blocked_and_its_worktree_untouched() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    then_a_reused_id_on_a_replaced_board_is_blocked(world, when_the_board_is_wiped);
}

#[test]
fn a_task_reusing_an_id_on_another_board_is_blocked_despite_a_stray_database_in_rider_home() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_stray_board_database_in_rider_home(&world);
    then_a_reused_id_on_a_replaced_board_is_blocked(world, when_rider_moves_to_another_board);
}

#[test]
fn a_task_reusing_an_id_on_a_board_reinitialised_in_place_is_blocked() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_the_board_keeps_its_data_in_rider_home(&mut world);
    then_a_reused_id_on_a_replaced_board_is_blocked(
        world,
        when_the_board_is_reinitialised_in_place,
    );
}

#[test]
fn the_daemon_blocks_a_task_whose_worktree_has_no_board_record_and_leaves_it_untouched() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let dir = given_a_leftover_worktree(&world, &task, &format!("rider/{task}-add-the-work"));
    fs::write(dir.join("left.txt"), "left behind\n").expect("write a leftover file");
    let before = worktree_state(&world, &dir);

    given_a_running_daemon(&mut world);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["blocked"]);
    let text = task_text(&world, &task);
    assert!(text.contains("rider_error"), "{text}");
    assert!(text.contains(dir.to_str().expect("UTF-8 path")), "{text}");
    assert_eq!(agent_runs_for(&world, &task), vec![]);
    assert_eq!(worktree_state(&world, &dir), before);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_task_based_on_a_task_it_does_not_need_is_blocked_as_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let stacked = NewTask::new("Add the stacked work.")
        .label("repo", world.repo_label())
        .label("base", &built)
        .role("implement");
    let stacked = when_the_lead_creates(&world, &stacked);
    given_a_running_daemon(&mut world);

    let states = when_the_tasks_settle(&world, &[&built, &stacked]);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(states, vec!["completed", "blocked"]);
    let reason = format!(
        "Add task #{built} to this task's needs, or remove the `base` label: a task is cut \
         from its `base` task's branch only once that task is done."
    );
    assert_eq!(
        task_block(&world, &stacked),
        Some(("invalid_brief".to_string(), reason))
    );
    assert_eq!(agent_runs_for(&world, &stacked), vec![]);
}

#[test]
fn an_onto_task_that_does_not_need_an_unfinished_task_on_its_branch_is_blocked() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let asking = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, false),
    );
    given_the_agent_will(&world, &asking, &[Behaviour::Block]);
    given_a_running_daemon(&mut world);
    assert_eq!(
        when_the_tasks_settle(&world, &[&built, &asking]),
        vec!["completed", "blocked"]
    );

    let racing = a_fix(&world, "Fix the work again.", &built, &built, false);
    let racing = when_the_lead_creates(&world, &racing);
    let states = when_the_tasks_settle(&world, &[&racing]);
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let reason = format!(
        "Add task #{asking} to this task's needs: it is unfinished on the branch of task \
         #{built}, which this task works `onto`."
    );
    assert_eq!(
        task_block(&world, &racing),
        Some(("invalid_brief".to_string(), reason))
    );
    assert_eq!(agent_runs_for(&world, &racing), vec![]);
}

#[test]
fn a_review_that_does_not_need_the_task_it_checks_is_blocked() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let review = NewTask::new("Review the work.")
        .label("repo", world.repo_label())
        .label("of", &built)
        .role("review");
    let review = when_the_lead_creates(&world, &review);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &review]);

    assert_eq!(
        (states, exit),
        (
            vec!["completed".to_string(), "blocked".to_string()],
            Some(0)
        )
    );
    let reason = format!(
        "Add task #{built} to this task's needs, or change the `of` label: a task checks \
         the work of its `of` task only once that task is done."
    );
    assert_eq!(
        task_block(&world, &review),
        Some(("invalid_brief".to_string(), reason))
    );
    assert_eq!(agent_runs_for(&world, &review), vec![]);
}

#[test]
fn an_onto_task_that_rebases_away_its_branchs_earlier_commits_fails() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, false),
    );
    given_the_agent_will(&world, &fix, &[Behaviour::DropEarlier]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&built, &fix]);

    assert_eq!(states, vec!["completed", "failed"]);
    assert_eq!(exit, Some(0));
    let text = task_text(&world, &fix);
    let reason = "no committed work: the branch no longer contains its starting commit";
    assert!(text.contains(reason), "{text}");
}

#[test]
fn a_restack_task_that_rebases_onto_the_named_commit_completes() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&built]), vec!["completed"]);
    fs::write(world.repo.join("moved.txt"), "main moved on\n").expect("write to main");
    world.git_ok(&["add", "moved.txt"]);
    world.git_ok(&["commit", "-q", "-m", "Move main on."]);
    let moved = world.git_ok(&["rev-parse", MAIN]);

    let restack =
        a_fix(&world, "Restack the work onto main.", &built, &built, false).label("restack", "1");
    let restack = when_the_lead_creates(&world, &restack);
    given_the_agent_will(&world, &restack, &[Behaviour::RebaseOntoMain]);
    let states = when_the_tasks_settle(&world, &[&restack]);
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let branch = task_label(&world, &built, "branch").expect("the built task's branch");
    assert!(is_ancestor(&world, &moved, &branch), "{branch} has {moved}");
    assert_eq!(
        branch_head_subject(&world, &branch),
        fake_agent::work_subject(&restack, 0)
    );
}

#[test]
fn an_onto_task_that_does_not_need_an_unfinished_task_onto_a_completed_fix_is_blocked() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fixed = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, false),
    );
    given_a_running_daemon(&mut world);
    assert_eq!(
        when_the_tasks_settle(&world, &[&built, &fixed]),
        vec!["completed", "completed"]
    );
    let gate = when_the_lead_creates(&world, &NewTask::new("Finish a prerequisite."));
    let waiting = a_fix(&world, "Fix the fix.", &fixed, &gate, false);
    let waiting = when_the_lead_creates(&world, &waiting);
    assert_eq!(task_state(&world, &waiting), "waiting");

    let racing = a_fix(&world, "Fix the work again.", &built, &built, false);
    let racing = when_the_lead_creates(&world, &racing);
    let states = when_the_tasks_settle(&world, &[&racing]);
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let reason = format!(
        "Add task #{waiting} to this task's needs: it is unfinished on the branch of task \
         #{built}, which this task works `onto`."
    );
    assert_eq!(
        task_block(&world, &racing),
        Some(("invalid_brief".to_string(), reason))
    );
    assert_eq!(agent_runs_for(&world, &racing), vec![]);
}

#[test]
fn an_unfinished_task_on_a_same_named_branch_of_another_repository_does_not_block_a_fix() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&built]), vec!["completed"]);
    let branch = task_label(&world, &built, "branch").expect("the built task's branch");
    let other = world.root.join("other-repository");
    let (from, to) = (world.repo_label(), other.to_str().expect("UTF-8 path"));
    let cloned = world.git(
        &world.root,
        &["clone", "--quiet", "--no-hardlinks", from, to],
    );
    assert_eq!(cloned.code, Some(0), "{cloned:?}");
    assert_eq!(
        world.git(&other, &["branch", &branch, "HEAD"]).code,
        Some(0)
    );
    let (repo, branch) = (format!("repo={to}"), format!("branch={branch}"));
    let create = [
        "tasks",
        "create",
        "work/other",
        "-",
        "--label",
        &repo,
        "--label",
        &branch,
    ];
    world.vb_lead(&create, Some("Finish work in another repository."));

    let fix = a_fix(&world, "Fix the work.", &built, &built, false);
    let fix = when_the_lead_creates(&world, &fix);
    let states = when_the_tasks_settle(&world, &[&fix]);
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    assert_eq!(task_block(&world, &fix), None);
}

#[test]
fn an_onto_task_is_blocked_when_the_open_tasks_are_too_many_to_check_its_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&built]), vec!["completed"]);
    let mut fix = a_fix(&world, "Fix the work.", &built, &built, false);
    let role = fix.role.take().expect("a fix has a role");
    let fix = when_the_lead_creates(&world, &fix);
    let plan: Vec<serde_json::Value> = (0..256)
        .map(|index| json!({"key": format!("idle-{index}"), "text": "Wait for other work."}))
        .collect();
    let file = world.root.join("idle-plan.json");
    fs::write(&file, json!(plan).to_string()).expect("write the plan");
    let file = file.to_str().expect("UTF-8 path");
    world.vb_lead(&["tasks", "create", "work/other", "--file", file], None);

    world.vb_lead(&["label", &fix, &format!("role={role}")], None);
    let states = when_the_tasks_settle(&world, &[&fix]);
    when_the_daemon_is_sent_sigterm(&world);
    let exit = then_the_daemon_exits(&mut world);

    assert_eq!((states, exit), (vec!["blocked".to_string()], Some(0)));
    let (kind, reason) = task_block(&world, &fix).expect("the fix's block");
    assert_eq!(kind, "invalid_brief");
    let start = "Finish or cancel unfinished tasks, or remove the `onto` label: ";
    let end = format!("so it cannot check which are unfinished on the branch of task #{built}.");
    assert!(
        reason.starts_with(start) && reason.ends_with(&end),
        "{reason}"
    );
    assert_eq!(agent_runs_for(&world, &fix), vec![]);
}

#[test]
fn rider_up_starts_the_daemon_in_the_background_and_rider_down_stops_it() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };

    let up = when_rider_runs(&world, &["up"]);
    assert_eq!(up.code, Some(0), "{up:?}");
    assert!(up.stdout.contains("board up"), "{up:?}");
    let pid = daemon_pid(&world);
    assert!(is_alive(pid), "the daemon {pid} is not running");
    let status = when_rider_runs(&world, &["status"]);
    assert_eq!(status.code, Some(0), "{status:?}");
    assert!(status.stdout.contains("board up"), "{status:?}");

    let down = when_rider_runs(&world, &["down"]);

    assert_eq!(down.code, Some(0), "{down:?}");
    assert!(
        when_the_process_ends_within(pid, 30),
        "the daemon {pid} still runs"
    );
    let after = when_rider_runs(&world, &["status"]);
    assert_eq!(after.code, Some(1), "{after:?}");
    assert!(after.stderr.contains("not running"), "{after:?}");
}

#[test]
fn a_client_that_sends_requests_without_reading_is_sent_whole_lines_ending_in_lagged() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let mut slow = given_a_client_on_the_daemon_socket(&world);

    let sent = when_the_client_sends_status_requests_without_reading(&mut slow);
    let status = when_rider_runs(&world, &["status"]);
    let lines = then_the_client_reads_to_the_end(&slow);

    assert_eq!(status.code, Some(0), "{status:?}");
    assert!(status.stdout.contains("board up"), "{status:?}");
    let last = lines
        .last()
        .expect("the daemon sent the lagging client a line");
    assert_eq!(last["ok"], json!(false), "{last}");
    assert_eq!(last["error"]["code"], json!("lagged"), "{last}");
    let answered = u64::try_from(lines.len()).expect("a line count fits in u64");
    assert!(answered <= sent, "{answered} lines for {sent} requests");
    let after = when_rider_runs(&world, &["status"]);
    assert_eq!(after.code, Some(0), "{after:?}");
    assert!(after.stdout.contains("board up"), "{after:?}");
}

#[test]
fn rider_down_drains_the_running_agent_claims_nothing_new_and_the_daemon_exits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let running = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &running, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &running);

    let down = thread::scope(|scope| {
        let down = scope.spawn(|| when_rider_runs(&world, &["down"]));
        assert!(
            when_rider_status_shows_within(&world, "draining", 30),
            "rider status never said draining"
        );
        let later = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
        assert_eq!(
            when_the_task_leaves_ready_within(&world, &later, 3),
            "ready"
        );
        world.release_agent(&running);
        let output = down.join().expect("rider down ran");
        assert_eq!(task_state(&world, &later), "ready");
        assert!(agent_runs_for(&world, &later).is_empty());
        output
    });

    assert_eq!(down.code, Some(0), "{down:?}");
    assert!(down.stdout.contains(&format!("#{running}")), "{down:?}");
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &running), "completed");
}

#[test]
fn rider_down_now_stops_the_agent_and_leaves_its_task_ready_with_attempt_unchanged() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    let run = when_the_agent_has_started(&world, &task);

    let down = when_rider_runs(&world, &["down", "--now"]);

    assert_eq!(down.code, Some(0), "{down:?}");
    assert!(down.stdout.contains(&format!("#{task}")), "{down:?}");
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(!is_alive(run.pid), "the agent {} still runs", run.pid);
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_label(&world, &task, "attempt"), None);
}

#[test]
fn rider_status_shows_a_running_and_a_waiting_task_and_rider_agents_the_running_one() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nbuilders_at_once = 1\n");
    let running = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let waiting = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
    given_the_agent_will(&world, &running, &[Behaviour::Hold]);
    given_the_agent_will(&world, &waiting, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &running);

    let status = when_rider_runs(&world, &["status"]);
    let agents = when_rider_runs(&world, &["agents"]);

    assert_eq!(status.code, Some(0), "{status:?}");
    assert!(status.stdout.contains("1 of 32 running"), "{status:?}");
    let line = |id: &str| {
        let found = status
            .stdout
            .lines()
            .find(|line| line.contains(&format!("#{id} ")));
        found.unwrap_or_else(|| panic!("no line for #{id}:\n{}", status.stdout))
    };
    let running_line = line(&running);
    assert!(running_line.contains("claimed"), "{running_line}");
    assert!(running_line.contains("implement-1-opus"), "{running_line}");
    let waiting_line = line(&waiting);
    assert!(waiting_line.contains("ready"), "{waiting_line}");
    assert!(waiting_line.contains("implement"), "{waiting_line}");
    assert_eq!(agents.code, Some(0), "{agents:?}");
    let rows: Vec<&str> = agents
        .stdout
        .lines()
        .filter(|l| l.starts_with('#'))
        .collect();
    assert_eq!(rows.len(), 1, "{agents:?}");
    assert!(
        rows[0].starts_with(&format!("#{running} implement implement-1-")),
        "{agents:?}"
    );
    assert!(rows[0].contains("running"), "{agents:?}");
    world.release_agent(&running);
    world.release_agent(&waiting);
    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn rider_status_groups_a_task_open_before_the_daemon_started_under_its_project() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let roleless = NewTask::new("Wait for a role.")
        .label("repo", world.repo_label())
        .label("from", MAIN);
    let task = when_the_lead_creates(&world, &roleless);
    given_a_running_daemon(&mut world);

    let listed = when_rider_status_shows_within(&world, &format!("#{task} "), 30);

    assert!(listed, "rider status never listed #{task}");
    let status = when_rider_runs(&world, &["status"]);
    let lines: Vec<&str> = status.stdout.lines().collect();
    let at = (lines.iter()).position(|line| line.starts_with(&format!("  #{task} ")));
    let heading = at.and_then(|at| lines[..at].iter().rev().find(|line| !line.starts_with(' ')));
    assert_eq!(heading, Some(&PROJECT), "{status:?}");
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn rider_logs_prints_a_completed_tasks_log_and_follows_a_running_one_until_its_agent_exits() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let done = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let running = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
    given_the_agent_will(&world, &running, &[Behaviour::HoldTalking]);
    given_a_running_daemon(&mut world);
    assert_eq!(when_the_tasks_settle(&world, &[&done]), vec!["completed"]);
    when_the_agent_has_started(&world, &running);

    let logs = when_rider_runs(&world, &["logs", &done]);
    let mut follow = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    follow.args(["logs", "-f", &running]).stdout(Stdio::piped());
    let mut follower = follow.spawn().expect("start rider logs -f");
    let lines = lines_of(follower.stdout.take().expect("rider logs -f output"));

    assert_eq!(logs.code, Some(0), "{logs:?}");
    assert!(
        logs.stdout.contains(&fake_agent::summary(&done, 0)),
        "{logs:?}"
    );
    let first = lines.recv_timeout(Duration::from_secs(10));
    assert_eq!(first, Ok(fake_agent::hold_line(&running)));
    assert_eq!(follower.try_wait().expect("poll rider logs -f"), None);
    world.release_agent(&running);
    let rest: Vec<String> = lines.iter().collect();
    let status = follower.wait().expect("wait for rider logs -f");
    assert_eq!(status.code(), Some(0));
    let summary = fake_agent::summary(&running, 0);
    assert!(rest.iter().any(|line| line.contains(&summary)), "{rest:?}");
    assert_eq!(
        when_the_tasks_settle(&world, &[&running]),
        vec!["completed"]
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    let without = when_rider_runs(&world, &["logs", &running]);
    assert_eq!(without.code, Some(0), "{without:?}");
    assert!(without.stdout.contains(&summary), "{without:?}");
}

#[test]
fn rider_kill_stops_the_agent_and_fails_its_task_killed_by_the_owner() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nbuilders_at_once = 1\n");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let waiting = when_the_lead_creates(&world, &an_implement_task(&world, "Add more work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_the_agent_will(&world, &waiting, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    let run = when_the_agent_has_started(&world, &task);

    let not_running = when_rider_runs(&world, &["kill", &waiting]);
    let unknown = when_rider_runs(&world, &["kill", "999999"]);
    let kill = when_rider_runs(&world, &["kill", &task]);

    assert_eq!(kill.code, Some(0), "{kill:?}");
    assert!(
        when_the_process_ends_within(run.pid, 10),
        "the agent {} still runs",
        run.pid
    );
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["failed"]);
    let read = world.vb_lead(&["read", &task, "--json"], None);
    assert!(read.contains("killed by the owner"), "{read}");
    assert_eq!(not_running.code, Some(1), "{not_running:?}");
    assert!(
        not_running.stderr.contains("not_running"),
        "{not_running:?}"
    );
    assert_eq!(unknown.code, Some(1), "{unknown:?}");
    assert!(unknown.stderr.contains("unknown_task"), "{unknown:?}");
    world.release_agent(&waiting);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
}

#[test]
fn rider_kill_of_a_resumed_agent_that_refused_its_session_starts_no_fresh_agent() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = a_fix(&world, "Fix the work.", &built, &built, true);
    let fix = when_the_lead_creates(&world, &fix);
    given_the_agent_will(
        &world,
        &fix,
        &[Behaviour::RefuseResumeThenHold, Behaviour::Hold],
    );
    given_a_running_daemon(&mut world);
    let refusing = when_the_agent_has_started(&world, &fix);
    when_the_log_shows(&world, &fix, "error_during_execution");

    let kill = when_rider_runs(&world, &["kill", &fix]);

    assert_eq!(kill.code, Some(0), "{kill:?}");
    assert!(when_the_process_ends_within(refusing.pid, 10));
    assert_eq!(when_the_tasks_settle(&world, &[&fix]), vec!["failed"]);
    let runs = agent_runs_for(&world, &fix);
    assert_eq!(
        runs.len(),
        1,
        "a fresh agent started after the kill: {runs:?}"
    );
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_label(&world, &fix, "attempt"), Some("1".to_string()));
}

#[test]
fn rider_logs_follow_moves_on_to_the_fresh_start_when_a_resumed_session_is_refused() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = a_fix(&world, "Fix the work.", &built, &built, true);
    let fix = when_the_lead_creates(&world, &fix);
    let plan = [Behaviour::RefuseResumeThenHold, Behaviour::HoldTalking];
    given_the_agent_will(&world, &fix, &plan);
    given_a_running_daemon(&mut world);
    when_the_agent_has_started(&world, &fix);
    when_the_log_shows(&world, &fix, "error_during_execution");

    let mut follow = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    follow.args(["logs", "-f", &fix]).stdout(Stdio::piped());
    let mut follower = follow.spawn().expect("start rider logs -f");
    let lines = lines_of(follower.stdout.take().expect("rider logs -f output"));
    let first: Vec<String> = (0..2)
        .map_while(|_| lines.recv_timeout(Duration::from_secs(10)).ok())
        .collect();
    assert!(
        first
            .iter()
            .any(|line| line.contains("No conversation found")),
        "{first:?}"
    );
    world.release_agent(&fix);
    let rest: Vec<String> = lines.iter().collect();

    assert_eq!(
        follower.wait().expect("wait for rider logs -f").code(),
        Some(0)
    );
    assert!(rest.contains(&fake_agent::hold_line(&fix)), "{rest:?}");
    let summary = fake_agent::summary(&fix, 1);
    assert!(rest.iter().any(|line| line.contains(&summary)), "{rest:?}");
    assert_eq!(when_the_tasks_settle(&world, &[&fix]), vec!["completed"]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn rider_kill_of_a_task_the_owner_canceled_writes_nothing_to_the_task() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon(&mut world);
    let run = when_the_agent_has_started(&world, &task);
    world.vb_lead(&["tasks", "cancel", &task, "superseded", "--json"], None);
    let (replies, labels) = (task_replies(&world, &task), task_labels(&world, &task));

    let kill = when_rider_runs(&world, &["kill", &task]);

    assert_eq!(kill.code, Some(0), "{kill:?}");
    assert!(when_the_process_ends_within(run.pid, 10));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(task_state(&world, &task), "canceled");
    assert_eq!(task_replies(&world, &task), replies);
    assert_eq!(task_labels(&world, &task), labels);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

const LOG_POLL_MILLIS: u64 = 50;
/// The board's shortest lease, with time for it to notice the lapse.
const LEASE_LAPSE_SECONDS_MAX: u64 = 90;
const LEASE_POLL_MILLIS: u64 = 500;

#[test]
fn a_daemon_that_crashes_while_its_agent_runs_resumes_the_claim_in_the_same_worktree() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::Commit]);
    given_a_running_daemon(&mut world);
    let leftover = when_the_agent_has_started(&world, &task);
    when_agents_json_names(&world, leftover.pid);
    let epoch = task_epoch(&world, &task);
    when_the_daemon_crashes(&mut world);
    assert!(is_alive(leftover.pid), "the agent outlived the crash");

    given_a_running_daemon(&mut world);

    assert!(
        when_the_process_ends_within(leftover.pid, 10),
        "the leftover agent {} still runs:\n{}",
        leftover.pid,
        world.daemon_log()
    );
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    let runs = agent_runs_for(&world, &task);
    assert_eq!(runs.len(), 2, "{}", world.daemon_log());
    assert_eq!(runs[1].dir, runs[0].dir);
    assert!(agent_prompt(&world, &task, runs[1].attempt).contains("This is a retry"));
    assert_eq!(task_epoch(&world, &task), epoch);
    assert_eq!(task_label(&world, &task, "attempt"), None);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

#[test]
fn a_daemon_that_crashes_past_its_shortened_lease_claims_the_task_afresh_and_completes_it() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold, Behaviour::Commit]);
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "lease=3000,tick=200")]);
    let leftover = when_the_agent_has_started(&world, &task);
    when_agents_json_names(&world, leftover.pid);
    let epoch = task_epoch(&world, &task);
    let holder = task_claim_profile(&world, &task).expect("the task is claimed");
    when_the_daemon_crashes(&mut world);
    // Rider claims for an hour; the board's shortest lease is 30 s.
    let renew = ["tasks", "renew", &task, "--lease", "30s", "--json"];
    let renewed = world.vb_as(&holder, &renew, None);
    assert_eq!(renewed.code, Some(0), "{renewed:?}");
    when_the_board_lease_lapses(&world, &task);

    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "lease=3000,tick=200")]);

    assert!(
        when_the_process_ends_within(leftover.pid, 10),
        "the leftover agent {} still runs:\n{}",
        leftover.pid,
        world.daemon_log()
    );
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    assert_eq!(agent_runs_for(&world, &task).len(), 2);
    assert_eq!(task_epoch(&world, &task), epoch + 1);
    assert_eq!(task_label(&world, &task, "attempt"), None);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
}

/// Waits until the board has let the claim on `task` lapse and shows it ready.
fn when_the_board_lease_lapses(world: &World, task: &str) {
    let deadline = Instant::now() + Duration::from_secs(LEASE_LAPSE_SECONDS_MAX);
    while task_state(world, task) != "ready" {
        assert!(
            Instant::now() < deadline,
            "#{task}'s claim never lapsed: {}",
            task_state(world, task)
        );
        thread::sleep(Duration::from_millis(LEASE_POLL_MILLIS));
    }
}

/// Waits until the daemon's log contains `text`.
fn when_the_daemon_log_shows(world: &World, text: &str) {
    let log = world.home.join("daemon.log");
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !fs::read_to_string(&log).unwrap_or_default().contains(text) {
        assert!(
            Instant::now() < deadline,
            "the daemon log never showed {text}:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(LOG_POLL_MILLIS));
    }
}

/// Waits until the board shows `task` in `state`.
fn when_the_task_is(world: &World, task: &str, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while task_state(world, task) != state {
        assert!(
            Instant::now() < deadline,
            "#{task} was {}, never {state}:\n{}",
            task_state(world, task),
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(LEASE_POLL_MILLIS));
    }
}

/// Waits until the first attempt log of `task` contains `text`.
fn when_the_log_shows(world: &World, task: &str, text: &str) {
    let log = world.home.join("logs").join(task).join("1.jsonl");
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !fs::read_to_string(&log).unwrap_or_default().contains(text) {
        assert!(
            Instant::now() < deadline,
            "{} never showed {text}",
            log.display()
        );
        thread::sleep(Duration::from_millis(LOG_POLL_MILLIS));
    }
}

/// The lines `output` prints, as they come, until it closes.
fn lines_of(output: impl Read + Send + 'static) -> Receiver<String> {
    let (lines, received) = sync_channel(1024);
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    received
}

fn daemon_pid(world: &World) -> u32 {
    let text = fs::read_to_string(world.home.join("daemon.lock")).expect("read daemon.lock");
    let pid = text
        .split_whitespace()
        .nth(1)
        .and_then(|pid| pid.parse().ok());
    pid.unwrap_or_else(|| panic!("no pid in daemon.lock: {text:?}"))
}

#[test]
fn two_tasks_completed_during_one_wait_are_both_named_in_its_message() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let first = when_the_lead_creates(&world, &NewTask::new("Write the first part."));
    let second = when_the_lead_creates(&world, &NewTask::new("Write the second part."));
    given_the_lead_has_waited(&world);
    when_a_worker_claims(&world, "w-1", &first);
    when_a_worker_claims(&world, "w-2", &second);

    let wait = when_rider_wait_starts(&world, &["--timeout", "1"]);
    when_the_worker_completes(&world, "w-1", &first);
    when_the_worker_completes(&world, "w-2", &second);
    let output = then_rider_wait_ends(wait);

    assert_eq!(output.code, Some(0), "{output:?}");
    let expected = format!(
        "completed #{first} Write the first part.\n\
         completed #{second} Write the second part.\nidle\n"
    );
    assert_eq!(output.stdout, expected);
    let again = when_rider_runs(&world, &["wait", PROJECT, "--timeout", "0"]);
    assert_eq!(again.stdout, "idle\n", "an event is never named twice");
}

#[test]
fn an_event_the_lead_caused_does_not_end_the_wait() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let held = when_the_lead_creates(&world, &NewTask::new("Hold this part back."));
    let done = when_the_lead_creates(&world, &NewTask::new("Write this part."));
    given_the_lead_has_waited(&world);
    when_a_worker_claims(&world, "w-1", &done);

    let wait = when_rider_wait_starts(&world, &["--timeout", "1"]);
    world.vb_lead(&["tasks", "block", &held, "held by the lead"], None);
    when_the_worker_completes(&world, "w-1", &done);
    let output = then_rider_wait_ends(wait);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(
        output.stdout,
        format!("completed #{done} Write this part.\n")
    );
}

#[test]
fn a_wait_that_times_out_prints_nothing_and_exits_with_its_own_code() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &NewTask::new("Write the slow part."));
    given_the_lead_has_waited(&world);
    when_a_worker_claims(&world, "w-1", &task);

    let output = when_rider_runs(&world, &["wait", PROJECT, "--timeout", "0"]);

    assert_eq!(output.code, Some(WAIT_TIMED_OUT), "{output:?}");
    assert_eq!(output.stdout, "");
    assert_eq!(output.stderr, "");
}

#[test]
fn a_wait_that_cannot_print_its_message_leaves_the_events_for_the_next_wait() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &NewTask::new("Write the lost part."));
    given_the_lead_has_waited(&world);
    when_a_worker_claims(&world, "w-1", &task);

    let mut wait = when_rider_wait_starts(&world, &["--timeout", "1"]);
    drop(wait.stdout.take());
    when_the_worker_completes(&world, "w-1", &task);
    let output = then_rider_wait_ends(wait);

    assert_eq!(output.code, Some(1), "{output:?}");
    let again = when_rider_runs(&world, &["wait", PROJECT, "--timeout", "0"]);
    assert_eq!(
        again.stdout,
        format!("completed #{task} Write the lost part.\nidle\n")
    );
}

#[test]
fn a_second_wait_on_one_project_is_refused_while_the_first_runs() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &NewTask::new("Write the shared part."));
    given_the_lead_has_waited(&world);
    when_a_worker_claims(&world, "w-1", &task);
    let first = when_rider_wait_starts(&world, &["--timeout", "1"]);
    let first_pid = first.id();
    when_the_wait_follows_the_feed(first_pid);

    let second = when_rider_runs(&world, &["wait", PROJECT, "--timeout", "0"]);
    when_the_worker_completes(&world, "w-1", &task);
    let first_output = then_rider_wait_ends(first);

    assert_eq!(second.code, Some(1), "{second:?}");
    assert_eq!(second.stdout, "");
    assert!(
        second
            .stderr
            .contains(&format!("is held by pid {first_pid}")),
        "{second:?}"
    );
    assert_eq!(
        first_output.stdout,
        format!("completed #{task} Write the shared part.\nidle\n")
    );
}

#[test]
fn a_wait_sent_sigterm_stops_its_feed_before_it_exits() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &NewTask::new("Write the stopped part."));
    given_the_lead_has_waited(&world);
    when_a_worker_claims(&world, "w-1", &task);
    let wait = when_rider_wait_starts(&world, &["--timeout", "1"]);
    let feeds = when_the_wait_follows_the_feed(wait.id());

    when_the_process_is_sent(wait.id(), "-TERM");
    let output = then_rider_wait_ends(wait);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert_eq!(output.stdout, "");
    for feed in feeds {
        assert!(
            when_the_process_ends_within(feed, 10),
            "vb tail {feed} outlived the wait"
        );
    }
}
