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

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use fake_agent::Behaviour;
use steps::*;
use world::{MAIN, PROJECT};

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
fn a_second_daemon_or_pool_on_the_same_home_exits_naming_the_holder() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let holder = world
        .pool
        .as_ref()
        .expect("the daemon")
        .child
        .id()
        .to_string();

    let daemon = when_rider_runs(&world, &["daemon"]);
    let pool = when_rider_runs(&world, &["pool", PROJECT]);

    for second in [daemon, pool] {
        assert_eq!(second.code, Some(1), "{second:?}");
        assert!(
            second.stderr.contains(&format!("pid {holder}")),
            "{second:?}"
        );
    }
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
    when_the_daemon_is_sent_sigterm(&world);

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
    when_the_daemon_is_sent_sigterm(&world);

    assert!(
        when_the_process_ends_within(run.pid, 5),
        "the agent {} still runs:\n{}",
        run.pid,
        world.pool_log()
    );
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_label(&world, &task, "attempt"), None);
}

#[test]
fn three_ready_polish_tasks_never_run_more_than_one_polish_agent_at_once() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let tasks: Vec<String> = [
        "Polish the first work.",
        "Polish the second work.",
        "Polish the third work.",
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
    given_a_rider_toml(&world, "[roles.polish]\nslots = 1\n");
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "tick=200")]);
    when_the_agent_has_started(&world, &tasks[0]);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(agents_running(&world), 1);

    for task in &tasks {
        world.release_agent(task);
    }

    let tasks: Vec<&str> = tasks.iter().map(String::as_str).collect();
    assert_eq!(when_the_tasks_settle(&world, &tasks), vec!["completed"; 3]);
    assert_eq!(world.most_agents_at_once(), 1);
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
    given_a_running_daemon_with(&mut world, &[(TEST_MILLIS_VAR, "lease=3000,tick=200")]);
    when_the_agent_has_started(&world, &task);
    let first = task_claim_until(&world, &task).expect("the task is claimed");

    thread::sleep(Duration::from_secs(4));

    let later = task_claim_until(&world, &task).expect("the task is still claimed");
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
    assert_eq!(task_label(&world, &task, "attempt"), Some("1".to_string()));
    assert_eq!(agent_runs_for(&world, &task).len(), 1);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
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
        world.pool_log()
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
fn a_second_sigterm_while_a_failure_is_being_written_releases_the_task_uncounted() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::ExitNonZero]);
    let marker = world.root.join("reply-started");
    given_vb_replies_slowly(&world, &marker, 3);
    given_a_running_daemon(&mut world);
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "no reply began:\n{}",
            world.pool_log()
        );
        thread::sleep(Duration::from_millis(100));
    }

    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert_eq!(task_state(&world, &task), "ready");
    assert_eq!(task_label(&world, &task, "attempt"), None);
    let log = fs::read_to_string(world.home.join("daemon.log")).expect("read daemon.log");
    assert!(log.contains("instead of settling: stop-now"), "{log}");
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
