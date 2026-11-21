use std::fs;
use std::path::{Path, PathBuf};

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{MAIN, World, write_executable};

const PR_URL: &str = "https://example.invalid/pull/1";

struct Remote {
    origin: PathBuf,
    gh: PathBuf,
}

fn given_an_origin_and_a_fake_gh(world: &World) -> Remote {
    let origin = world.root.join("origin.git");
    let gh = world.root.join("gh");
    fs::create_dir_all(&gh).expect("create the fake gh's directory");
    let made = world.git(&world.root, &["init", "-q", "--bare", "origin.git"]);
    assert_eq!(made.code, Some(0), "{made:?}");
    let origin_text = origin.to_str().expect("UTF-8 path");
    world.git_ok(&["remote", "add", "origin", origin_text]);
    world.git_ok(&["push", "-q", "origin", MAIN]);
    let script = format!(
        r#"#!/bin/sh
dir='{dir}'
printf '%s\n' "$*" >> "$dir/calls"
case "$1 $2" in
"pr list")
  if [ -f "$dir/created" ]; then printf '[{{"url":"{PR_URL}"}}]\n'; else printf '[]\n'; fi ;;
"pr create")
  cat > "$dir/body"; : > "$dir/created"; printf '%s\n' '{PR_URL}' ;;
*) exit 9 ;;
esac
"#,
        dir = gh.display()
    );
    write_executable(&world.agents.join("gh"), &script);
    Remote { origin, gh }
}

fn given_a_completed_task(world: &mut World) -> (String, String) {
    let task = when_the_lead_creates(world, &an_implement_task(world, "Add one work file."));
    given_the_agent_will(world, &task, &[Behaviour::Commit]);
    let (states, _) = when_the_daemon_runs_until_settled(world, &[&task]);
    assert_eq!(states, vec!["completed".to_string()]);
    let branch = task_label(world, &task, "branch").expect("the task has a branch label");
    (task, branch)
}

fn origin_head(world: &World, origin: &Path, branch: &str) -> Option<String> {
    let found = world.git(origin, &["rev-parse", "--verify", "-q", branch]);
    (found.code == Some(0)).then(|| found.stdout.trim_end().to_string())
}

fn gh_calls(remote: &Remote) -> Vec<String> {
    let calls = fs::read_to_string(remote.gh.join("calls")).unwrap_or_default();
    calls.lines().map(str::to_string).collect()
}

#[test]
fn rider_pr_pushes_a_completed_tasks_branch_and_opens_one_draft_with_its_summary() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);

    let first = when_rider_runs(&world, &["pr", &task]);
    let second = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(first.stdout, format!("{PR_URL}\n"));
    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(second.stdout, format!("{PR_URL}\n"));
    let head = branch_head(&world, &branch);
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(head));
    let body = fs::read_to_string(remote.gh.join("body")).expect("read the PR body");
    assert_eq!(body, fake_agent::summary(&task, 0));
    let list = format!("pr list --head {branch} --state open --json url --limit 2");
    let create =
        format!("pr create --draft --head {branch} --title=Add one work file. --body-file -");
    assert_eq!(gh_calls(&remote), vec![list.clone(), create, list]);
}

#[test]
fn rider_pr_refuses_a_branch_that_moved_on_origin_and_opens_no_pr() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    world.git_ok(&["checkout", "-q", "-b", "elsewhere", MAIN]);
    world.git_ok(&[
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "Someone else's commit",
    ]);
    world.git_ok(&[
        "push",
        "-q",
        "origin",
        &format!("elsewhere:refs/heads/{branch}"),
    ]);
    let moved = branch_head(&world, "elsewhere");

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("git push"), "{output:?}");
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(moved));
    assert_eq!(gh_calls(&remote), Vec::<String>::new());
}

#[test]
fn rider_pr_refuses_a_task_that_is_not_completed_and_pushes_nothing() {
    let Some(world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("is not completed"), "{output:?}");
    assert_eq!(gh_calls(&remote), Vec::<String>::new());
}
