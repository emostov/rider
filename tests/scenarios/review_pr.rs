use std::fs;

use crate::fake_agent::{self, Behaviour, PR_REF};
use crate::steps::*;
use crate::world::{MAIN, World};

const REMOTE_MAIN: &str = "origin/main";
const PR_FILE: &str = "pr.txt";

struct PullRequest {
    main: String,
    head: String,
}

fn given_a_fetched_pr_behind_origin_main(world: &World) -> PullRequest {
    let remote = world.root.join("remote.git");
    let remote = remote.to_str().expect("UTF-8 path");
    let bare = world.git(&world.root, &["init", "-q", "--bare", "-b", MAIN, remote]);
    assert_eq!(bare.code, Some(0), "{bare:?}");
    world.git_ok(&["remote", "add", "origin", remote]);
    world.git_ok(&["checkout", "-q", "--detach"]);
    fs::write(world.repo.join(PR_FILE), "the pull request\n").expect("write the PR's file");
    world.git_ok(&["add", PR_FILE]);
    world.git_ok(&["commit", "-q", "-m", "Add the pull request's file"]);
    let head = world.git_ok(&["rev-parse", "HEAD"]);
    world.git_ok(&["push", "-q", "origin", "HEAD:refs/pull/412/head"]);
    world.git_ok(&["checkout", "-q", MAIN]);
    fs::write(world.repo.join("main.txt"), "main moved on\n").expect("write main's file");
    world.git_ok(&["add", "main.txt"]);
    world.git_ok(&["commit", "-q", "-m", "Move main on"]);
    world.git_ok(&["push", "-q", "origin", MAIN]);
    let fetched = format!("refs/pull/412/head:{PR_REF}");
    world.git_ok(&["fetch", "-q", "origin", MAIN, &fetched]);
    let main = world.git_ok(&["rev-parse", "--verify", REMOTE_MAIN]);
    assert_eq!(world.git_ok(&["rev-parse", "--verify", PR_REF]), head);
    assert!(!is_ancestor(world, &main, &head) && !is_ancestor(world, &head, &main));
    PullRequest { main, head }
}

fn parents(world: &World, commit: &str) -> Vec<String> {
    let line = world.git_ok(&["rev-list", "--parents", "-n", "1", commit]);
    line.split(' ').skip(1).map(str::to_string).collect()
}

#[test]
fn a_pr_merged_onto_main_is_reviewed_at_the_merge_and_synthesised_on_its_own_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let pr = given_a_fetched_pr_behind_origin_main(&world);
    let merge = NewTask::new("Merge the pull request onto main.")
        .label("repo", world.repo_label())
        .label("from", REMOTE_MAIN)
        .role("light");
    let merge = when_the_lead_creates(&world, &merge);
    given_the_agent_will(&world, &merge, &[Behaviour::MergePr]);
    let review = NewTask::new("Review the pull request.")
        .label("repo", world.repo_label())
        .label("of", &merge)
        .needs(&merge)
        .role("review");
    let review = when_the_lead_creates(&world, &review);
    given_the_agent_will(&world, &review, &[Behaviour::RecordHead]);
    let synthesis = NewTask::new("Write the review file.")
        .label("repo", world.repo_label())
        .label("base", &merge)
        .needs(&merge)
        .needs(&review)
        .role("implement");
    let synthesis = when_the_lead_creates(&world, &synthesis);

    let tasks = [merge.as_str(), review.as_str(), synthesis.as_str()];
    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &tasks);

    assert_eq!(states, vec!["completed", "completed", "completed"]);
    assert_eq!(exit, Some(0));
    let merged = task_label(&world, &merge, "branch").expect("the merge task has a branch");
    let merge_head = branch_head(&world, &merged);
    assert_eq!(parents(&world, &merge_head), vec![pr.main.clone(), pr.head]);
    assert_eq!(task_label(&world, &merge, "start"), Some(pr.main.clone()));
    let changed = world.git_ok(&["diff", "--name-only", &pr.main, &merge_head]);
    assert_eq!(changed, PR_FILE);
    assert_eq!(
        fake_agent::head(&world.root, &review, 0),
        (merge_head.clone(), None)
    );
    assert_eq!(task_label(&world, &review, "branch"), None);
    let synthesised = then_the_branch_has_the_agents_commit(&world, &synthesis, 0);
    assert_ne!(synthesised, merged);
    let synthesis_head = branch_head(&world, &synthesised);
    assert_eq!(parents(&world, &synthesis_head), vec![merge_head.clone()]);
    assert_eq!(
        task_label(&world, &synthesis, "start"),
        Some(merge_head.clone())
    );
    assert_eq!(branches(&world), {
        let mut expected = vec![MAIN.to_string(), merged, synthesised];
        expected.sort();
        expected
    });
    then_only_the_repository_is_a_worktree(&world);
}
