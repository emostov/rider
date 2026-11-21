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
    let untrusted = an_untrusted_task(&world, "Review the pull request.", "implement");
    let untrusted = when_the_lead_creates(&world, &untrusted);
    given_the_agent_will(&world, &untrusted, &[Behaviour::CommitNothing]);
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
    assert!(!world.home.join("untrusted").join(&untrusted).exists());
}

#[test]
fn an_untrusted_label_other_than_1_is_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_untrusted_task(&world, "Review the pull request.", "implement")
        .label("untrusted", "yes");
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
    let untrusted = an_untrusted_task(&world, "Review the pull request.", "implement");
    let untrusted = when_the_lead_creates(&world, &untrusted);
    given_the_agent_will(&world, &untrusted, &[Behaviour::CommitNothing]);
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
        "GIT_CEILING_DIRECTORIES",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
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

fn an_untrusted_task(world: &World, text: &str, role: &str) -> NewTask {
    NewTask::new(text)
        .label("repo", world.repo_label())
        .label("untrusted", "1")
        .role(role)
}

#[test]
fn an_untrusted_task_gets_a_scratch_directory_and_never_the_prs_code_in_git() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let log = given_tracked_hooks_and_a_fetched_pr(&world);
    let branches_before = branches(&world);
    let merge = when_the_lead_creates(
        &world,
        &an_untrusted_task(&world, "Merge the pull request.", "light"),
    );
    given_the_agent_will(&world, &merge, &[Behaviour::MergePr]);
    let synthesis = an_untrusted_task(&world, "Write the review.", "implement");
    let synthesis = when_the_lead_creates(&world, &synthesis);
    given_the_agent_will(&world, &synthesis, &[Behaviour::CommitNothing]);
    let review = an_untrusted_task(&world, "Review the pull request.", "review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::CommitNothing]);

    let tasks = [merge.as_str(), synthesis.as_str(), review.as_str()];
    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["failed", "completed", "completed"]);
    assert!(
        !log.exists(),
        "{}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(branches(&world), branches_before);
    let worktrees = world.git_ok(&["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
    for task in tasks {
        let run = &agent_runs_for(&world, task)[0];
        let scratch = world.home.join("untrusted").join(task).join("work");
        assert_eq!(run.dir, scratch);
        assert_eq!(task_label(&world, task, "branch"), None);
    }
}

#[test]
fn an_untrusted_task_with_a_branch_label_is_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = an_untrusted_task(&world, "Merge the pull request.", "light").label("from", MAIN);
    let task = when_the_lead_creates(&world, &task);

    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!(states, vec!["blocked"]);
    let reason = "Remove the `from` label: an untrusted task gets no git worktree or branch, \
                  only a scratch directory.";
    assert_eq!(
        task_block(&world, &task),
        Some(("invalid_brief".to_string(), reason.to_string()))
    );
}
