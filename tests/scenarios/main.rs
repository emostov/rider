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
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use fake_agent::Behaviour;
use steps::*;
use world::{MAIN, PROJECT, World};

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
fn a_pool_stopped_after_a_failed_release_of_a_task_whose_role_changed_retries_it_first() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[roles.light]\nslots = 1\n");
    let task = an_implement_task(&world, "Add the work.").role("light");
    let task = when_the_lead_creates(&world, &task);
    let releases = world.root.join("releases.log");
    given_the_role_changes_after_the_claim_and_one_release_fails(
        &world,
        &task,
        "light",
        "implement",
        &releases,
    );
    when_the_pool_starts(&mut world);
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !releases.exists() {
        assert!(Instant::now() < deadline, "{}", world.pool_log());
        thread::sleep(Duration::from_millis(100));
    }

    let exit = when_the_pool_is_stopped(&mut world);

    assert_eq!(exit, Some(0), "{}", world.pool_log());
    assert_eq!(task_state(&world, &task), "ready", "{}", world.pool_log());
    let tries = fs::read_to_string(&releases).expect("read the release log");
    assert_eq!(tries, "release\nrelease\n");
    assert_eq!(agent_runs_for(&world, &task), vec![]);
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
            world.pool_log()
        );
        thread::sleep(Duration::from_millis(100));
    }

    when_the_daemon_is_sent_sigterm(&world);
    when_the_daemon_is_sent_sigterm(&world);

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
    // released, so the fix is ready while the branch is still checked out.
    given_vb_runs_slowly(&world, "post", &world.root.join("posted"), 3);
    given_a_running_daemon(&mut world);

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
        world.pool_log()
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
    assert_eq!(agent_runs_for(&world, &task), vec![run]);
    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(
        !is_alive(boards[1]),
        "the board the daemon started outlived it"
    );
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
fn a_pool_gives_a_refused_resumes_fresh_start_only_what_remains_of_the_attempt_limit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_rider_toml(&world, "[limits]\nattempt_minutes = 1\n");
    let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let fix = when_the_lead_creates(
        &world,
        &a_fix(&world, "Fix the work.", &built, &built, true),
    );
    given_the_agent_will(&world, &fix, &[Behaviour::RefuseResumeSlowly]);

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&built, &fix]);

    assert_eq!(states, vec!["completed", "failed"], "{}", world.pool_log());
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

    let (states, exit) = when_the_pool_runs_until_settled(&mut world, &[&built, &fix]);

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
    let refusal = lines.recv_timeout(Duration::from_secs(10));
    assert!(
        refusal
            .as_ref()
            .is_ok_and(|line| line.contains("No conversation found")),
        "{refusal:?}"
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
        world.pool_log()
    );
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    let runs = agent_runs_for(&world, &task);
    assert_eq!(runs.len(), 2, "{}", world.pool_log());
    assert_eq!(runs[1].dir, runs[0].dir);
    assert!(agent_prompt(&world, &task, 2).contains("This is a retry"));
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
        world.pool_log()
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
