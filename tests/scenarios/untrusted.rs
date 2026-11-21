use std::fs;

use crate::fake_agent;
use crate::steps::*;

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
    let expected = [
        "BOARD_PROFILE",
        "BOARD_TASK",
        "BOARD_URL",
        "GH_CONFIG_DIR",
        "HOME",
        "PATH",
        "PWD",
        "RIDER_AGENT_ACK_FILE",
        "RIDER_AGENT_PID_FILE",
        "RIDER_AGENT_VB_DIR",
        "SHLVL",
        "_",
    ];
    assert_eq!(fake_agent::env_names(&world.root, &untrusted, 0), expected);
}
