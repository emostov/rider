use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::fake_agent::{self, Behaviour, PR_REF};
use crate::steps::*;
use crate::world::{MAIN, World};

const TOKENS: [(&str, &str); 3] = [
    ("GH_TOKEN", "gh-secret"),
    ("GITHUB_TOKEN", "github-secret"),
    ("GH_ENTERPRISE_TOKEN", "enterprise-secret"),
];

#[test]
fn an_untrusted_task_runs_without_github_tokens_and_with_an_empty_gh_config() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let trusted = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let untrusted = an_implement_task(&world, "Merge the pull request.").label("untrusted", "1");
    let untrusted = when_the_lead_creates(&world, &untrusted);
    given_a_running_daemon_with(&mut world, &TOKENS);

    let states = when_the_tasks_settle(&world, &[&trusted, &untrusted]);

    assert_eq!(states, vec!["completed".to_string(); 2]);
    let mut seen: Vec<String> = TOKENS.iter().map(|(k, v)| format!("{k}={v}")).collect();
    seen.sort();
    assert_eq!(fake_agent::gh_env(&world.root, &trusted, 0), seen);
    let config = world
        .home
        .join("untrusted")
        .join(&untrusted)
        .join("gh-config");
    let expected = vec![format!("GH_CONFIG_DIR={}", config.display())];
    assert_eq!(fake_agent::gh_env(&world.root, &untrusted, 0), expected);
    let entries = fs::read_dir(&config)
        .expect("the gh config directory exists")
        .count();
    assert_eq!(entries, 0);
}

#[test]
fn an_untrusted_label_other_than_1_is_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_implement_task(&world, "Merge the pull request.").label("untrusted", "yes");
    let task = when_the_lead_creates(&world, &task);

    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!(states, vec!["blocked"]);
    let reason = "Change the `untrusted` label from \"yes\" to 1, or remove it.".to_string();
    assert_eq!(
        task_block(&world, &task),
        Some(("invalid_brief".to_string(), reason))
    );
}

const SECRETS: [(&str, &str); 6] = [
    ("GH_TOKEN", "gh-secret"),
    ("GITHUB_TOKEN", "github-secret"),
    ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
    ("OPENAI_API_KEY", "openai-secret"),
    ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
    ("NPM_TOKEN", "npm-secret"),
];

#[test]
fn an_untrusted_task_sees_only_the_allowed_variables() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let trusted = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let untrusted = an_implement_task(&world, "Merge the pull request.").label("untrusted", "1");
    let untrusted = when_the_lead_creates(&world, &untrusted);
    given_a_running_daemon_with(&mut world, &SECRETS);

    let states = when_the_tasks_settle(&world, &[&trusted, &untrusted]);

    assert_eq!(states, vec!["completed".to_string(); 2]);
    let trusted_names = fake_agent::env_names(&world.root, &trusted, 0);
    assert!(
        SECRETS
            .iter()
            .all(|(name, _)| trusted_names.iter().any(|seen| seen == name))
    );
    let mut expected: Vec<String> = [
        "BOARD_PROFILE",
        "BOARD_TASK",
        "BOARD_URL",
        "GH_CONFIG_DIR",
        "GIT_ATTR_NOSYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_TERMINAL_PROMPT",
        "HOME",
        "PATH",
        "PWD",
        "RIDER_AGENT_ACK_FILE",
        "RIDER_AGENT_PID_FILE",
        "RIDER_AGENT_VB_DIR",
        "SHLVL",
        "_",
    ]
    .map(String::from)
    .into();
    for index in 0..12 {
        expected.push(format!("GIT_CONFIG_KEY_{index}"));
        expected.push(format!("GIT_CONFIG_VALUE_{index}"));
    }
    expected.sort();
    assert_eq!(fake_agent::env_names(&world.root, &untrusted, 0), expected);
}

const HOOKS: [&str; 3] = ["pre-commit", "pre-merge-commit", "post-merge"];

fn given_tracked_hooks_and_a_fetched_pr(world: &World) -> PathBuf {
    let log = world.root.join("hooks.log");
    let hooks = world.repo.join(".githooks");
    fs::create_dir_all(&hooks).expect("create the hooks directory");
    for hook in HOOKS {
        let script = format!("#!/bin/sh\necho \"{hook} $(pwd)\" >> '{}'\n", log.display());
        let path = hooks.join(hook);
        fs::write(&path, script).expect("write a hook");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod a hook");
    }
    world.git_ok(&["add", ".githooks"]);
    world.git_ok(&["commit", "-q", "--no-verify", "-m", "Track the hooks"]);
    world.git_ok(&["config", "core.hooksPath", ".githooks"]);
    world.git_ok(&["checkout", "-q", "--detach"]);
    fs::write(world.repo.join("pr.txt"), "the pull request\n").expect("write the PR's file");
    world.git_ok(&["add", "pr.txt"]);
    world.git_ok(&[
        "commit",
        "-q",
        "--no-verify",
        "-m",
        "Add the pull request's file",
    ]);
    world.git_ok(&["update-ref", PR_REF, "HEAD"]);
    world.git_ok(&["checkout", "-q", MAIN]);
    fs::write(world.repo.join("main.txt"), "main moved on\n").expect("write main's file");
    world.git_ok(&["add", "main.txt"]);
    world.git_ok(&["commit", "-q", "--no-verify", "-m", "Move main on"]);
    assert!(!log.exists());
    log
}

#[test]
fn an_untrusted_merge_and_commit_run_none_of_the_repositorys_tracked_hooks() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let log = given_tracked_hooks_and_a_fetched_pr(&world);
    let merge = NewTask::new("Merge the pull request onto main.")
        .label("repo", world.repo_label())
        .label("from", MAIN)
        .label("untrusted", "1")
        .role("light");
    let merge = when_the_lead_creates(&world, &merge);
    given_the_agent_will(&world, &merge, &[Behaviour::MergePr]);
    let synthesis = NewTask::new("Write the review file.")
        .label("repo", world.repo_label())
        .label("base", &merge)
        .label("untrusted", "1")
        .needs(&merge)
        .role("implement");
    let synthesis = when_the_lead_creates(&world, &synthesis);
    let trusted = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));

    let tasks = [merge.as_str(), synthesis.as_str(), trusted.as_str()];
    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed"; 3]);
    let ran = fs::read_to_string(&log).expect("the trusted commit ran its hook");
    let trusted_dir = worktree_dir(&world, &trusted);
    let expected = format!("pre-commit {}\n", trusted_dir.display());
    assert_eq!(ran, expected);
}
