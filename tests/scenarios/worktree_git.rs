use std::fs;
use std::path::Path;

use super::{a_fix, a_task_given_its_role_after};
use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{MAIN, World};

const COMMITTER: [&str; 4] = [
    "-c",
    "user.name=Scenario",
    "-c",
    "user.email=scenario@example.invalid",
];

fn git_in(world: &World, dir: &Path, args: &[&str]) -> String {
    let args: Vec<&str> = COMMITTER.iter().chain(args).copied().collect();
    let output = world.git(dir, &args);
    assert_eq!(output.code, Some(0), "git {args:?} failed: {output:?}");
    output.stdout.trim_end().to_string()
}

fn a_completed_task(world: &mut World) -> String {
    let built = when_the_lead_creates(world, &an_implement_task(world, "Add the work."));
    given_a_running_daemon(world);
    assert_eq!(when_the_tasks_settle(world, &[&built]), vec!["completed"]);
    built
}

fn then_it_is_blocked(world: &World, task: &str, kind: &str, reason: &str) {
    assert_eq!(when_the_tasks_settle(world, &[task]), vec!["blocked"]);
    let block = Some((kind.to_string(), reason.to_string()));
    assert_eq!(task_block(world, task), block);
    assert_eq!(agent_runs_for(world, task), vec![]);
    assert!(!worktree_dir(world, task).exists());
}

fn when_the_daemon_stops(world: &mut World) {
    when_the_daemon_is_sent_sigterm(world);
    assert_eq!(then_the_daemon_exits(world), Some(0));
}

#[test]
fn a_task_without_from_is_cut_from_origin_head_on_a_branch_named_by_its_first_line() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let main_head = world.git_ok(&["rev-parse", MAIN]);
    world.git_ok(&["update-ref", "refs/remotes/origin/main", MAIN]);
    let origin_head = [
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ];
    world.git_ok(&origin_head);
    world.git_ok(&["checkout", "-q", "-b", "elsewhere"]);
    git_in(
        &world,
        &world.repo,
        &["commit", "-q", "--allow-empty", "-m", "Elsewhere."],
    );
    let mut new = an_implement_task(&world, "  --Extract Token__Store!--\nThe details.");
    new.labels.retain(|(key, _)| key != "from");
    let task = when_the_lead_creates(&world, &new);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    assert_eq!(branch, format!("rider/{task}-extract-token-store"));
    assert_eq!(
        world.git_ok(&["rev-parse", &format!("{branch}~1")]),
        main_head
    );
    assert_eq!(
        agent_runs_for(&world, &task)[0].dir,
        worktree_dir(&world, &task)
    );
}

#[test]
fn a_task_without_from_in_a_repository_without_origin_head_is_blocked_as_an_invalid_brief() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut new = an_implement_task(&world, "Add the work.");
    new.labels.retain(|(key, _)| key != "from");
    let task = when_the_lead_creates(&world, &new);
    given_a_running_daemon(&mut world);

    let reason = "Set `from=<branch>` on this task to the branch to cut it from: it has no \
                  `base` or `onto`, and the repository has no origin/HEAD to default to.";
    then_it_is_blocked(&world, &task, "invalid_brief", reason);
    when_the_daemon_stops(&mut world);
}

#[test]
fn a_branch_label_that_names_a_tag_head_an_option_or_too_long_a_ref_blocks_its_dependents() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    world.git_ok(&["tag", "v1"]);
    let long = format!("b/{}", "x".repeat(248));
    world.git_ok(&["branch", &long, MAIN]);
    given_a_running_daemon(&mut world);

    for label in ["v1", "HEAD", "-x", long.as_str()] {
        let built = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
        assert_eq!(when_the_tasks_settle(&world, &[&built]), vec!["completed"]);
        world.vb_lead(&["label", &built, &format!("branch={label}")], None);
        let based = NewTask::new("Add the stacked work.")
            .label("repo", world.repo_label())
            .label("base", &built)
            .needs(&built)
            .role("implement");
        let based = when_the_lead_creates(&world, &based);
        let reason = format!(
            "Set the `branch` label on task #{built} to its branch, or remove the `base` \
             label from this task."
        );
        then_it_is_blocked(&world, &based, "invalid_brief", &reason);
        let onto = a_fix(&world, "Fix the work.", &built, &built, false);
        let onto = when_the_lead_creates(&world, &onto);
        let reason = format!(
            "Set the `branch` label on task #{built} to its branch, or remove the `onto` \
             label from this task."
        );
        then_it_is_blocked(&world, &onto, "invalid_brief", &reason);
        let qa = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Run the work."));
        let reason = format!(
            "Set the `branch` label on task #{built} to the branch to check, or change the \
             `of` label to a task that has one."
        );
        then_it_is_blocked(&world, &qa, "invalid_brief", &reason);
    }
    when_the_daemon_stops(&mut world);
}

#[test]
fn an_onto_task_whose_branch_is_checked_out_in_the_repository_is_blocked_as_branch_busy() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = a_completed_task(&mut world);
    world.vb_lead(&["label", &built, &format!("branch={MAIN}")], None);

    let fix = when_the_lead_creates(&world, &a_fix(&world, "Fix it.", &built, &built, false));

    let reason = format!(
        "Wait until the branch of task #{built} is not checked out in another worktree, then \
         unblock this task."
    );
    then_it_is_blocked(&world, &fix, "branch_busy", &reason);
    when_the_daemon_stops(&mut world);
}

#[test]
fn a_new_task_whose_branch_is_checked_out_in_another_worktree_is_blocked_as_branch_busy() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut new = an_implement_task(&world, "Add the work.");
    new.role = None;
    let task = when_the_lead_creates(&world, &new);
    let branch = format!("rider/{task}-add-the-work");
    let elsewhere = world.root.join("elsewhere");
    let path = elsewhere.to_str().expect("UTF-8 path");
    world.git_ok(&["worktree", "add", "-q", "-b", &branch, "--", path, MAIN]);
    given_a_running_daemon(&mut world);

    world.vb_lead(&["label", &task, "role=implement"], None);

    let reason = format!(
        "Wait until branch {branch} is not checked out in another worktree, then unblock this \
         task."
    );
    then_it_is_blocked(&world, &task, "branch_busy", &reason);
    when_the_daemon_stops(&mut world);
}

fn when_a_held_agent_finishes_after(world: &mut World, change: impl Fn(&World, &Path)) -> String {
    let task = a_task_given_its_role_after(world, "Add the work.", Behaviour::Hold);
    given_a_running_daemon(world);
    let run = when_the_agent_has_started(world, &task);
    change(world, &run.dir);
    world.release_agent(&task);
    assert_eq!(when_the_tasks_settle(world, &[&task]), vec!["completed"]);
    when_the_daemon_stops(world);
    task
}

fn then_the_worktree_is_kept_because(world: &World, task: &str, why: &str) {
    let dir = worktree_dir(world, task);
    assert!(dir.is_dir(), "the worktree {} is kept", dir.display());
    let branch = task_label(world, task, "branch").expect("the task has a branch label");
    let note = format!(
        "Kept the worktree at {}, so branch {branch} stays checked out there: tasks working \
         `onto` it will block as branch_busy until a person removes or fixes the worktree at \
         {}. It was kept because {why}.",
        dir.display(),
        dir.display()
    );
    let replies: Vec<String> = task_replies(world, task)
        .iter()
        .map(|reply| reply.to_string())
        .collect();
    assert!(
        replies.iter().any(|reply| reply.contains(&note)),
        "no reply holds {note:?}: {replies:?}"
    );
}

#[test]
fn a_finished_worktree_with_a_bisect_in_progress_is_kept_and_named_in_the_thread() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };

    let task = when_a_held_agent_finishes_after(&mut world, |world, dir| {
        git_in(world, dir, &["bisect", "start"]);
    });

    then_the_worktree_is_kept_because(&world, &task, "an operation is in progress");
}

#[test]
fn a_finished_worktree_with_a_commit_only_its_head_reflog_reaches_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };

    let task = when_a_held_agent_finishes_after(&mut world, |world, dir| {
        let branch = git_in(world, dir, &["symbolic-ref", "--short", "HEAD"]);
        git_in(world, dir, &["checkout", "-q", "--detach"]);
        git_in(
            world,
            dir,
            &["commit", "-q", "--allow-empty", "-m", "Orphan."],
        );
        git_in(world, dir, &["checkout", "-q", &branch]);
    });

    then_the_worktree_is_kept_because(&world, &task, "commits only its HEAD reflog reaches");
}

#[test]
fn a_finished_worktree_holding_only_ignored_build_output_is_removed_and_its_branch_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };

    let task = when_a_held_agent_finishes_after(&mut world, |world, dir| {
        fs::write(dir.join(".gitignore"), "target/\n").expect("write .gitignore");
        git_in(world, dir, &["add", "--", ".gitignore"]);
        git_in(world, dir, &["commit", "-q", "-m", "Ignore target."]);
        fs::create_dir_all(dir.join("target")).expect("create target");
        fs::write(dir.join("target").join("build.log"), "output\n").expect("write the log");
    });

    then_the_worktree_is_removed(&world, &task);
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    let ignore = file_at(&world, &branch, ".gitignore");
    assert_eq!(ignore, Some("target/\n".to_string()));
}

fn when_a_task_settles_after_a_hold(
    world: &mut World,
    task: &NewTask,
    change: impl Fn(&World, &Path),
) -> (String, String) {
    let id = when_the_lead_creates(world, task);
    given_the_agent_will(world, &id, &[Behaviour::HoldThenNothing]);
    given_a_running_daemon(world);
    let run = when_the_agent_has_started(world, &id);
    change(world, &run.dir);
    world.release_agent(&id);
    let state = when_the_tasks_settle(world, &[&id]).remove(0);
    when_the_daemon_stops(world);
    (id, state)
}

fn then_the_task_failed_because(world: &World, task: &str, state: &str, reason: &str) {
    assert_eq!(state, "failed");
    let text = task_text(world, task);
    assert!(text.contains(reason), "no {reason:?} in {text}");
    assert!(worktree_dir(world, task).is_dir(), "the worktree is kept");
}

fn then_the_detached_worktree_is_kept_because(world: &World, task: &str, why: &str) {
    let dir = worktree_dir(world, task);
    assert!(dir.is_dir(), "the worktree {} is kept", dir.display());
    let note = format!("Kept the worktree at {}: {why}.", dir.display());
    let replies: Vec<String> = task_replies(world, task)
        .iter()
        .map(|reply| reply.to_string())
        .collect();
    assert!(
        replies.iter().any(|reply| reply.contains(&note)),
        "no reply holds {note:?}: {replies:?}"
    );
}

fn head_of(world: &World, dir: &Path) -> String {
    git_in(world, dir, &["rev-parse", "HEAD"])
}

#[test]
fn a_daemon_started_with_an_inherited_git_dir_works_only_in_the_tasks_repository() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let decoy = world.root.join("decoy");
    fs::create_dir_all(&decoy).expect("create the decoy");
    git_in(&world, &decoy, &["init", "-q", "-b", "decoy"]);
    git_in(
        &world,
        &decoy,
        &["commit", "-q", "--allow-empty", "-m", "Decoy."],
    );
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let decoy_git = decoy.join(".git");
    let decoy_git = decoy_git.to_str().expect("UTF-8 path");
    given_a_running_daemon_with(&mut world, &[("GIT_DIR", decoy_git)]);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_stops(&mut world);

    then_the_branch_has_the_agents_commit(&world, &task, 0);
    then_the_worktree_is_removed(&world, &task);
    let decoy_branches = git_in(&world, &decoy, &["branch", "--format=%(refname:short)"]);
    assert_eq!(decoy_branches, "decoy");
    let decoy_worktrees = git_in(&world, &decoy, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        decoy_worktrees
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1
    );
}

#[test]
fn an_agent_that_leaves_an_untracked_file_fails_and_its_worktree_is_kept_with_the_file() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::LeaveUncommitted]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!(exit, Some(0));
    let reason = "the worktree has uncommitted changes";
    then_the_task_failed_because(&world, &task, &states[0], reason);
    let left = worktree_dir(&world, &task).join(format!("uncommitted-{task}.txt"));
    assert_eq!(
        fs::read_to_string(left).ok(),
        Some("left behind\n".to_string())
    );
}

#[test]
fn an_agent_that_leaves_a_modified_file_fails_and_its_worktree_is_kept_with_the_change() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let new = an_implement_task(&world, "Add the work.");

    let (task, state) = when_a_task_settles_after_a_hold(&mut world, &new, |world, dir| {
        fs::write(dir.join("notes.txt"), "committed\n").expect("write the notes");
        git_in(world, dir, &["add", "--", "notes.txt"]);
        git_in(world, dir, &["commit", "-q", "-m", "Notes."]);
        fs::write(dir.join("notes.txt"), "modified\n").expect("modify the notes");
    });

    let reason = "the worktree has uncommitted changes";
    then_the_task_failed_because(&world, &task, &state, reason);
    let notes = worktree_dir(&world, &task).join("notes.txt");
    assert_eq!(
        fs::read_to_string(notes).ok(),
        Some("modified\n".to_string())
    );
}

#[test]
fn a_repo_label_naming_a_subdirectory_works_in_its_repository_named_by_its_last_component() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let sub = world.repo.join("crates").join("store");
    fs::create_dir_all(&sub).expect("create the subdirectory");
    fs::write(sub.join("lib.rs"), "pub struct Store;\n").expect("write the crate");
    git_in(&world, &world.repo, &["add", "--", "crates"]);
    git_in(
        &world,
        &world.repo,
        &["commit", "-q", "-m", "Add the store crate."],
    );
    let main_head = world.git_ok(&["rev-parse", MAIN]);
    world.git_ok(&["update-ref", "refs/remotes/origin/main", MAIN]);
    let origin_head = [
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ];
    world.git_ok(&origin_head);
    let new = NewTask::new("Add the work.")
        .label("repo", sub.to_str().expect("UTF-8 path"))
        .role("implement");
    let task = when_the_lead_creates(&world, &new);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let dir = world.home.join("worktrees").join("store").join(&task);
    assert_eq!(agent_runs_for(&world, &task)[0].dir, dir);
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    assert_eq!(
        world.git_ok(&["rev-parse", &format!("{branch}~1")]),
        main_head
    );
    assert!(!dir.exists(), "the worktree is removed");
}

fn when_a_failed_task_is_reopened_after(
    world: &mut World,
    task: &NewTask,
    change: impl Fn(&World, &Path),
) -> String {
    let id = when_the_lead_creates(world, task);
    given_the_agent_will(
        world,
        &id,
        &[Behaviour::ExitNonZero, Behaviour::CommitNothing],
    );
    assert_eq!(when_the_tasks_settle(world, &[&id]), vec!["failed"]);
    change(world, &worktree_dir(world, &id));
    world.vb_lead(&["tasks", "reopen", &id], None);
    id
}

fn then_it_is_blocked_reusing(world: &World, task: &str, on: &str, expected: &str) {
    assert_eq!(when_the_tasks_settle(world, &[task]), vec!["blocked"]);
    let dir = worktree_dir(world, task);
    let reason = format!(
        "Rider cannot prepare the worktree: {} is on {on}, not on {expected}.",
        dir.display()
    );
    assert_eq!(
        task_block(world, task),
        Some(("rider_error".to_string(), reason))
    );
    assert_eq!(agent_runs_for(world, task).len(), 1);
}

#[test]
fn a_reopened_task_whose_worktree_left_its_branch_is_blocked_and_its_worktree_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let new = an_implement_task(&world, "Add the work.");
    let task = when_a_failed_task_is_reopened_after(&mut world, &new, |world, dir| {
        git_in(world, dir, &["switch", "-q", "-c", "other"]);
    });
    let expected = format!("branch rider/{task}-add-the-work");
    then_it_is_blocked_reusing(&world, &task, "branch other", &expected);

    let new = an_implement_task(&world, "Add more work.");
    let detached = when_a_failed_task_is_reopened_after(&mut world, &new, |world, dir| {
        git_in(world, dir, &["checkout", "-q", "--detach"]);
    });
    let expected = format!("branch rider/{detached}-add-more-work");
    then_it_is_blocked_reusing(&world, &detached, "a detached HEAD", &expected);
    when_the_daemon_stops(&mut world);
    let on = git_in(
        &world,
        &worktree_dir(&world, &task),
        &["branch", "--show-current"],
    );
    assert_eq!(on, "other");
}

#[test]
fn a_reopened_review_whose_checkout_took_a_branch_is_blocked_and_its_checkout_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = a_completed_task(&mut world);
    let qa = a_qa_task(&world, &built, "Run the work.");
    let qa = when_a_failed_task_is_reopened_after(&mut world, &qa, |world, dir| {
        git_in(world, dir, &["switch", "-q", "-c", "notes"]);
    });

    then_it_is_blocked_reusing(&world, &qa, "branch notes", "a detached HEAD");
    when_the_daemon_stops(&mut world);
}

#[test]
fn a_reopened_review_reuses_its_moved_checkout_untouched_and_keeps_the_commit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = a_completed_task(&mut world);
    let qa = a_qa_task(&world, &built, "Run the work.");
    let qa = when_a_failed_task_is_reopened_after(&mut world, &qa, |world, dir| {
        git_in(
            world,
            dir,
            &["commit", "-q", "--allow-empty", "-m", "Notes."],
        );
    });
    let noted = head_of(&world, &worktree_dir(&world, &qa));

    assert_eq!(when_the_tasks_settle(&world, &[&qa]), vec!["completed"]);
    when_the_daemon_stops(&mut world);

    let runs = agent_runs_for(&world, &qa);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].dir, runs[0].dir);
    assert_eq!(head_of(&world, &runs[1].dir), noted);
    then_the_detached_worktree_is_kept_because(&world, &qa, "commits only its HEAD reflog reaches");
}

#[test]
fn a_tag_named_like_the_tasks_branch_does_not_stop_its_worktree_being_removed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };

    let task = when_a_held_agent_finishes_after(&mut world, |world, dir| {
        let branch = git_in(world, dir, &["branch", "--show-current"]);
        git_in(world, &world.repo, &["tag", &branch, MAIN]);
    });

    then_the_worktree_is_removed(&world, &task);
    let branch = format!("refs/heads/rider/{task}-add-the-work");
    let subject = world.git_ok(&["log", "-1", "--format=%s", &branch]);
    assert_eq!(subject, fake_agent::work_subject(&task, 0));
}

#[test]
fn a_stacked_task_is_cut_from_its_base_branch_and_not_a_tag_of_the_same_name() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = a_completed_task(&mut world);
    let branch = task_label(&world, &built, "branch").expect("the task has a branch label");
    world.git_ok(&["tag", &branch, MAIN]);
    let stacked = NewTask::new("Add the stacked work.")
        .label("repo", world.repo_label())
        .label("base", &built)
        .needs(&built)
        .role("implement");
    let stacked = when_the_lead_creates(&world, &stacked);

    assert_eq!(
        when_the_tasks_settle(&world, &[&stacked]),
        vec!["completed"]
    );
    when_the_daemon_stops(&mut world);

    let base_head = world.git_ok(&["rev-parse", &format!("refs/heads/{branch}")]);
    let stacked_branch = then_the_branch_has_the_agents_commit(&world, &stacked, 0);
    let parent = world.git_ok(&["rev-parse", &format!("{stacked_branch}~1")]);
    assert_eq!(parent, base_head);
}

#[test]
fn a_worktree_moved_off_its_branch_fails_its_task_and_keeps_the_worktree_and_commit() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let new = an_implement_task(&world, "Add the work.");

    let (task, state) = when_a_task_settles_after_a_hold(&mut world, &new, |world, dir| {
        git_in(
            world,
            dir,
            &["commit", "-q", "--allow-empty", "-m", "On the branch."],
        );
        git_in(world, dir, &["checkout", "-q", "--detach"]);
        git_in(
            world,
            dir,
            &["commit", "-q", "--allow-empty", "-m", "Off the branch."],
        );
    });

    let branch = format!("rider/{task}-add-the-work");
    let reason = format!("the worktree is not on its branch {branch}");
    then_the_task_failed_because(&world, &task, &state, &reason);
    let dir = worktree_dir(&world, &task);
    let subject = git_in(&world, &dir, &["log", "-1", "--format=%s"]);
    assert_eq!(subject, "Off the branch.");
}

#[test]
fn a_review_whose_checkout_moved_fails_and_keeps_the_checkout_where_it_moved() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = a_completed_task(&mut world);
    when_the_daemon_stops(&mut world);
    let qa = a_qa_task(&world, &built, "Run the work.");

    let (qa, state) = when_a_task_settles_after_a_hold(&mut world, &qa, |world, dir| {
        git_in(world, dir, &["checkout", "-q", "--detach", "HEAD~1"]);
    });

    then_the_task_failed_because(&world, &qa, &state, "review changed the checkout");
    let main_head = world.git_ok(&["rev-parse", MAIN]);
    assert_eq!(head_of(&world, &worktree_dir(&world, &qa)), main_head);
}

#[test]
fn a_review_checkout_whose_head_only_it_reaches_without_a_head_reflog_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    world.git_ok(&["config", "core.logAllRefUpdates", "false"]);
    let built = a_completed_task(&mut world);
    when_the_daemon_stops(&mut world);
    let branch = task_label(&world, &built, "branch").expect("the task has a branch label");
    let qa = a_qa_task(&world, &built, "Run the work.");

    let (qa, state) = when_a_task_settles_after_a_hold(&mut world, &qa, |world, _| {
        git_in(world, &world.repo, &["branch", "-q", "-D", &branch]);
    });

    assert_eq!(state, "completed");
    then_the_detached_worktree_is_kept_because(&world, &qa, "commits only its HEAD reflog reaches");
}

#[test]
fn a_review_checkout_with_a_commit_only_an_old_head_reflog_id_reaches_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    world.git_ok(&["config", "core.logAllRefUpdates", "false"]);
    let built = a_completed_task(&mut world);
    when_the_daemon_stops(&mut world);
    let qa = a_qa_task(&world, &built, "Run the work.");

    let (qa, state) = when_a_task_settles_after_a_hold(&mut world, &qa, |world, dir| {
        let start = head_of(world, dir);
        git_in(
            world,
            dir,
            &["commit", "-q", "--allow-empty", "-m", "Orphan."],
        );
        git_in(
            world,
            &world.repo,
            &["config", "core.logAllRefUpdates", "true"],
        );
        git_in(world, dir, &["checkout", "-q", "--detach", &start]);
    });

    assert_eq!(state, "completed");
    then_the_detached_worktree_is_kept_because(&world, &qa, "commits only its HEAD reflog reaches");
}

fn when_the_checkout_changes_after_completion(
    world: &mut World,
    task: &str,
    change: impl Fn(&World, &Path),
) {
    let marker = world.root.join("result-posted");
    let release = world.root.join("release-result");
    given_vb_waits_for(world, "rider-result@2", &marker, &release);
    given_a_running_daemon(world);
    when_the_file_exists(world, &marker);
    assert_eq!(task_state(world, task), "completed");
    change(world, &worktree_dir(world, task));
    fs::write(&release, "").expect("release the result post");
    when_the_daemon_stops(world);
}

fn then_a_reply_holds(world: &World, task: &str, text: &str) {
    let replies: Vec<String> = task_replies(world, task)
        .iter()
        .map(|reply| reply.to_string())
        .collect();
    assert!(
        replies.iter().any(|reply| reply.contains(text)),
        "no reply holds {text:?}: {replies:?}"
    );
}

#[test]
fn a_file_left_in_a_completed_tasks_worktree_before_its_removal_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));

    when_the_checkout_changes_after_completion(&mut world, &task, |_, dir| {
        fs::write(dir.join("precious.txt"), "keep me\n").expect("write precious.txt");
    });

    let kept = fs::read_to_string(worktree_dir(&world, &task).join("precious.txt"));
    assert_eq!(kept.ok(), Some("keep me\n".to_string()));
    then_a_reply_holds(&world, &task, "Kept the worktree at");
}

#[test]
fn a_completed_tasks_worktree_switched_to_another_branch_before_its_removal_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));

    when_the_checkout_changes_after_completion(&mut world, &task, |world, dir| {
        git_in(world, dir, &["switch", "-q", "-c", "other"]);
    });

    assert!(worktree_dir(&world, &task).is_dir(), "the worktree is kept");
    then_a_reply_holds(&world, &task, "It was kept because not on its branch.");
}

#[test]
fn a_completed_reviews_checkout_moved_before_its_removal_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let built = a_completed_task(&mut world);
    when_the_daemon_stops(&mut world);
    let qa = when_the_lead_creates(&world, &a_qa_task(&world, &built, "Run the work."));
    given_the_agent_will(&world, &qa, &[Behaviour::CommitNothing]);

    when_the_checkout_changes_after_completion(&mut world, &qa, |world, dir| {
        git_in(world, dir, &["checkout", "-q", "--detach", MAIN]);
    });

    then_the_detached_worktree_is_kept_because(&world, &qa, "moved from its start");
}

#[test]
fn a_completed_tasks_worktree_with_a_rebase_stopped_before_its_removal_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));

    when_the_checkout_changes_after_completion(&mut world, &task, |world, dir| {
        let stopped = world.git(dir, &["rebase", "-q", "--exec", "false", "HEAD~1"]);
        assert_ne!(stopped.code, Some(0), "the rebase stops: {stopped:?}");
    });

    then_the_detached_worktree_is_kept_because(&world, &task, "an operation is in progress");
}

#[test]
fn a_finished_worktree_with_a_cherry_pick_stopped_after_its_first_commit_is_kept() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    world.git_ok(&["checkout", "-q", "-b", "picks"]);
    fs::write(world.repo.join("work.txt"), "theirs\n").expect("write work.txt");
    world.git_ok(&["add", "--", "work.txt"]);
    git_in(&world, &world.repo, &["commit", "-q", "-m", "First pick."]);
    let first = world.git_ok(&["rev-parse", "HEAD"]);
    let empty = ["commit", "-q", "--allow-empty", "-m", "Second pick."];
    git_in(&world, &world.repo, &empty);
    let second = world.git_ok(&["rev-parse", "HEAD"]);
    world.git_ok(&["checkout", "-q", MAIN]);

    let task = when_a_held_agent_finishes_after(&mut world, |world, dir| {
        fs::write(dir.join("work.txt"), "ours\n").expect("write work.txt");
        git_in(world, dir, &["add", "--", "work.txt"]);
        git_in(world, dir, &["commit", "-q", "-m", "Our work."]);
        let picked = world.git(dir, &["cherry-pick", &first, &second]);
        assert_ne!(picked.code, Some(0), "the cherry-pick stops: {picked:?}");
        fs::write(dir.join("work.txt"), "resolved\n").expect("resolve work.txt");
        git_in(world, dir, &["commit", "-q", "-a", "--no-edit"]);
    });

    then_the_worktree_is_kept_because(&world, &task, "an operation is in progress");
}

#[test]
fn a_git_call_printing_past_its_output_bound_blocks_the_task_and_its_process_group_is_killed() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let pid_file = world.root.join("sleeper.pid");
    let git = format!(
        "#!/bin/sh\ncase \" $* \" in *\" worktree add \"*)\n  sleep 60 &\n  echo $! > '{}'\n  \
         head -c 4194305 /dev/zero\n  exec sleep 60 ;;\nesac\nexec /usr/bin/git \"$@\"\n",
        pid_file.display()
    );
    crate::world::write_executable(&world.tools.join("git"), &git);
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_a_running_daemon(&mut world);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["blocked"]);
    when_the_daemon_stops(&mut world);

    let block = task_block(&world, &task).expect("the task has a block");
    assert_eq!(block.0, "rider_error");
    assert!(
        block.1.contains("printed more than 4194304 bytes"),
        "{block:?}"
    );
    let pid = fs::read_to_string(&pid_file).expect("git wrote its sleeper's pid");
    let pid: u32 = pid.trim().parse().expect("a pid");
    assert!(
        when_the_process_ends_within(pid, 10),
        "git's background sleep {pid} outlived the git call"
    );
}

#[test]
fn a_task_from_a_branch_other_than_main_is_cut_from_that_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    world.git_ok(&["checkout", "-q", "-b", "feature"]);
    git_in(
        &world,
        &world.repo,
        &["commit", "-q", "--allow-empty", "-m", "Feature."],
    );
    let feature = world.git_ok(&["rev-parse", "HEAD"]);
    world.git_ok(&["checkout", "-q", MAIN]);
    let mut new = an_implement_task(&world, "Add the work.");
    new.labels.retain(|(key, _)| key != "from");
    let task = when_the_lead_creates(&world, &new.label("from", "feature"));

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    let branch = then_the_branch_has_the_agents_commit(&world, &task, 0);
    assert_eq!(
        world.git_ok(&["rev-parse", &format!("{branch}~1")]),
        feature
    );
}

#[test]
fn a_commit_another_local_branch_also_holds_is_not_the_agents_own() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let new = an_implement_task(&world, "Add the work.");

    let (task, state) = when_a_task_settles_after_a_hold(&mut world, &new, |world, dir| {
        git_in(
            world,
            dir,
            &["commit", "-q", "--allow-empty", "-m", "Borrowed."],
        );
        git_in(world, dir, &["branch", "sibling"]);
    });

    then_the_task_failed_because(
        &world,
        &task,
        &state,
        "the branch has no commits of its own",
    );
}

#[test]
fn a_retry_reuses_the_dirty_worktree_of_the_failed_attempt_untouched() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_a_running_daemon(&mut world);
    let new = an_implement_task(&world, "Add the work.");

    let task = when_a_failed_task_is_reopened_after(&mut world, &new, |_, dir| {
        fs::write(dir.join("README.md"), "precious modification\n").expect("modify README.md");
        fs::write(dir.join("precious.txt"), "precious untracked\n").expect("write precious.txt");
    });

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["failed"]);
    when_the_daemon_stops(&mut world);
    let runs = agent_runs_for(&world, &task);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].dir, worktree_dir(&world, &task));
    let dir = worktree_dir(&world, &task);
    let readme = fs::read_to_string(dir.join("README.md")).ok();
    assert_eq!(readme, Some("precious modification\n".to_string()));
    let untracked = fs::read_to_string(dir.join("precious.txt")).ok();
    assert_eq!(untracked, Some("precious untracked\n".to_string()));
}

#[test]
fn a_new_task_whose_branch_exists_without_a_worktree_continues_that_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let mut new = an_implement_task(&world, "Add the work.");
    new.role = None;
    let task = when_the_lead_creates(&world, &new);
    let branch = format!("rider/{task}-add-the-work");
    world.git_ok(&["checkout", "-q", "-b", &branch]);
    git_in(
        &world,
        &world.repo,
        &["commit", "-q", "--allow-empty", "-m", "Existing work."],
    );
    let existing = world.git_ok(&["rev-parse", "HEAD"]);
    world.git_ok(&["checkout", "-q", MAIN]);
    given_a_running_daemon(&mut world);

    world.vb_lead(&["label", &task, "role=implement"], None);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_stops(&mut world);
    assert_eq!(
        then_the_branch_has_the_agents_commit(&world, &task, 0),
        branch
    );
    assert_eq!(
        world.git_ok(&["rev-parse", &format!("{branch}~1")]),
        existing
    );
}

const INHERITED: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

#[test]
fn a_daemon_started_with_every_git_repository_variable_starts_agents_with_none_of_them() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    let foreign = world.root.join("foreign");
    let paths: Vec<String> = INHERITED
        .iter()
        .map(|name| foreign.join(name).to_str().expect("UTF-8 path").to_string())
        .collect();
    let env: Vec<(&str, &str)> = INHERITED
        .iter()
        .zip(&paths)
        .map(|(name, path)| (*name, path.as_str()))
        .collect();
    given_a_running_daemon_with(&mut world, &env);

    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_the_daemon_stops(&mut world);

    then_the_branch_has_the_agents_commit(&world, &task, 0);
    let seen = fake_agent::git_env(&world.root, &task, 0);
    let leaked: Vec<&String> = seen
        .iter()
        .filter(|name| INHERITED.contains(&name.as_str()))
        .collect();
    assert_eq!(leaked, Vec::<&String>::new(), "the agent saw {seen:?}");
    assert!(
        !foreign.exists(),
        "nothing was written to {}",
        foreign.display()
    );
}

#[test]
fn a_start_label_naming_no_commit_is_ignored() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let new = an_implement_task(&world, "Add the work.").label("start", &"0".repeat(40));
    let task = when_the_lead_creates(&world, &new);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    then_the_branch_has_the_agents_commit(&world, &task, 0);
}

#[test]
fn a_task_whose_heading_is_all_punctuation_gets_the_fallback_branch_name() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "!!! !!!"));

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    assert_eq!(
        then_the_branch_has_the_agents_commit(&world, &task, 0),
        format!("rider/{task}-task")
    );
}

#[test]
fn rider_never_runs_an_fsmonitor_an_agent_planted_in_the_repository_config() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add the work."));
    given_the_agent_will(&world, &task, &[Behaviour::PlantFsmonitor]);

    let (states, exit) = when_the_daemon_runs_until_settled(&mut world, &[&task]);

    assert_eq!((states, exit), (vec!["completed".to_string()], Some(0)));
    assert!(!fake_agent::fsmonitor_ran(&world.root, &task, 0));
}
