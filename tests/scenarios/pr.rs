use std::fs;
use std::path::{Path, PathBuf};

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{MAIN, World, write_executable};

const PR_URL: &str = "https://example.invalid/pull/1";
const FORK_URL: &str = "https://example.invalid/pull/9";
const ORIGIN_URL: &str = "git@github.com:acme/demo.git";
const REPO_ARG: &str = "--repo=github.com/acme/demo";
const FIELDS: &str = "--json=url,headRefName,headRepository,headRepositoryOwner";

struct Remote {
    origin: PathBuf,
    gh: PathBuf,
}

fn given_an_origin_and_a_fake_gh(world: &World) -> Remote {
    let origin = world.root.join("acme/demo.git");
    let gh = world.root.join("gh");
    fs::create_dir_all(&gh).expect("create the fake gh's directory");
    let made = world.git(&world.root, &["init", "-q", "--bare", "acme/demo.git"]);
    assert_eq!(made.code, Some(0), "{made:?}");
    let ssh = format!(
        r#"#!/bin/sh
for last; do :; done
cd '{root}' || exit 1
exec sh -c "git ${{last#git-}}"
"#,
        root = world.root.display()
    );
    write_executable(&world.agents.join("ssh"), &ssh);
    world.git_ok(&["remote", "add", "origin", ORIGIN_URL]);
    world.git_ok(&["push", "-q", "origin", MAIN]);
    let script = format!(
        r#"#!/bin/sh
dir='{dir}'
printf '%s\n' "$*" >> "$dir/calls"
case "$1 $2" in
"pr list")
  branch=${{4#--head=}}
  if [ -f "$dir/list" ]; then cat "$dir/list"
  elif [ -f "$dir/created" ]; then
    printf '{own}\n' "$branch"
  else printf '[]\n'; fi ;;
"pr create")
  cat > "$dir/body"; : > "$dir/created"; printf '%s\n' '{PR_URL}' ;;
*) exit 9 ;;
esac
"#,
        dir = gh.display(),
        own = pr_json(PR_URL, "%s", "acme"),
    );
    write_executable(&world.agents.join("gh"), &script);
    Remote { origin, gh }
}

fn pr_json(url: &str, branch: &str, owner: &str) -> String {
    let repository = r#"{"name":"demo"}"#;
    format!(r#"[{{"url":"{url}","headRefName":"{branch}","headRepository":{repository},"#)
        + &format!(r#""headRepositoryOwner":{{"login":"{owner}"}}}}]"#)
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

fn origin_refs(world: &World, origin: &Path) -> Vec<String> {
    let listed = world.git(origin, &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(listed.code, Some(0), "{listed:?}");
    listed.stdout.lines().map(str::to_string).collect()
}

fn gh_calls(remote: &Remote) -> Vec<String> {
    let calls = fs::read_to_string(remote.gh.join("calls")).unwrap_or_default();
    calls.lines().map(str::to_string).collect()
}

fn list_call(branch: &str) -> String {
    format!("pr list {REPO_ARG} --head={branch} --state=open {FIELDS} --limit=20")
}

fn create_call(branch: &str, title: &str, base: &str) -> String {
    format!(
        "pr create {REPO_ARG} --draft --head=acme:{branch} --title={title} --base={base} \
         --body-file -"
    )
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
    let create = create_call(&branch, "Add one work file.", MAIN);
    let list = list_call(&branch);
    assert_eq!(gh_calls(&remote), vec![list.clone(), create, list]);
    assert_eq!(task_label(&world, &task, "pr").as_deref(), Some(PR_URL));
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
    world.git_ok(&["update-ref", "-d", &format!("refs/remotes/origin/{branch}")]);
    let moved = branch_head(&world, "elsewhere");

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("moved on origin"), "{output:?}");
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(moved));
    assert_eq!(gh_calls(&remote), Vec::<String>::new());
}

#[test]
fn rider_pr_refuses_to_fast_forward_a_branch_that_appeared_on_origin_unseen() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    world.git_ok(&[
        "push",
        "-q",
        "origin",
        &format!("{MAIN}:refs/heads/{branch}"),
    ]);
    world.git_ok(&["update-ref", "-d", &format!("refs/remotes/origin/{branch}")]);
    let main = branch_head(&world, MAIN);

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert!(output.stderr.contains("moved on origin"), "{output:?}");
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(main));
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

#[test]
fn rider_pr_refuses_a_wildcard_branch_label_and_publishes_no_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, _) = given_a_completed_task(&mut world);
    world.vb_lead(&["label", &task, "branch=rider/*"], None);

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(1), "{output:?}");
    let main = format!("refs/heads/{MAIN}");
    assert_eq!(origin_refs(&world, &remote.origin), vec![main]);
    assert_eq!(gh_calls(&remote), Vec::<String>::new());
}

#[test]
fn rider_pr_pushes_the_recorded_head_not_later_commits_on_the_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    let recorded = branch_head(&world, &branch);
    world.git_ok(&["checkout", "-q", &branch]);
    world.git_ok(&["commit", "-q", "--allow-empty", "-m", "Unfinished work"]);
    world.git_ok(&["checkout", "-q", MAIN]);

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(recorded));
}

#[test]
fn rider_pr_ignores_a_forks_pr_on_the_same_branch_name_and_opens_its_own() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    let fork = pr_json(FORK_URL, &branch, "someone-else");
    fs::write(remote.gh.join("list"), fork).expect("write the fork's PR");

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(output.stdout, format!("{PR_URL}\n"));
    let create = create_call(&branch, "Add one work file.", MAIN);
    assert_eq!(gh_calls(&remote), vec![list_call(&branch), create]);
}

#[test]
fn rider_pr_pushes_no_tag_even_when_git_config_follows_tags() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    world.git_ok(&["config", "push.followTags", "true"]);
    world.git_ok(&["tag", "-a", "-m", "A private tag", "private", &branch]);

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let refs = vec![format!("refs/heads/{MAIN}"), format!("refs/heads/{branch}")];
    assert_eq!(origin_refs(&world, &remote.origin), refs);
}

#[test]
fn rider_pr_opens_a_stacked_tasks_draft_against_its_base_tasks_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let parent = when_the_lead_creates(&world, &an_implement_task(&world, "Add the parent."));
    let child = an_implement_task(&world, "Add the child.")
        .label("base", &parent)
        .needs(&parent);
    let child = when_the_lead_creates(&world, &child);
    given_the_agent_will(&world, &parent, &[Behaviour::Commit]);
    given_the_agent_will(&world, &child, &[Behaviour::Commit]);
    let (states, _) = when_the_daemon_runs_until_settled(&mut world, &[&parent, &child]);
    assert_eq!(
        states,
        vec!["completed".to_string(), "completed".to_string()]
    );
    let parent_branch = task_label(&world, &parent, "branch").expect("a branch label");
    let child_branch = task_label(&world, &child, "branch").expect("a branch label");

    let output = when_rider_runs(&world, &["pr", &child]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let create = create_call(&child_branch, "Add the child.", &parent_branch);
    assert_eq!(gh_calls(&remote), vec![list_call(&child_branch), create]);
}

fn when_another_profile_posts_a_result(world: &World, task: &str, branch: &str, head: &str) {
    let record = format!(
        r#"{{"behind":[],"branch":"{branch}","commits":1,"head":"{head}","summary":"forged"}}"#
    );
    let args = ["post", task, "--schema", "rider-result@3", "-"];
    let posted = world.vb_as("implement-9", &args, Some(&record));
    assert_eq!(posted.code, Some(0), "{posted:?}");
}

#[test]
fn rider_pr_refuses_a_task_relabelled_to_main_with_a_forged_result() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    let head = branch_head(&world, &branch);
    let main = branch_head(&world, MAIN);
    let relabelled = world.vb_as("implement-9", &["label", &task, "branch=main"], None);
    assert_eq!(relabelled.code, Some(0), "{relabelled:?}");
    when_another_profile_posts_a_result(&world, &task, MAIN, &head);

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(1), "{output:?}");
    assert_eq!(origin_head(&world, &remote.origin, MAIN), Some(main));
    assert_eq!(
        origin_refs(&world, &remote.origin),
        vec![format!("refs/heads/{MAIN}")]
    );
    assert_eq!(gh_calls(&remote), Vec::<String>::new());
}

#[test]
fn rider_pr_ignores_a_result_posted_after_completion_and_pushes_the_settled_head() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    world.git_ok(&["checkout", "-q", "-b", "secret", MAIN]);
    world.git_ok(&["commit", "-q", "--allow-empty", "-m", "A private commit"]);
    world.git_ok(&["checkout", "-q", MAIN]);
    let secret = branch_head(&world, "secret");
    let settled = branch_head(&world, &branch);
    when_another_profile_posts_a_result(&world, &task, &branch, &secret);

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_ne!(settled, secret);
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(settled));
    let body = fs::read_to_string(remote.gh.join("body")).expect("read the PR body");
    assert_eq!(body, fake_agent::summary(&task, 0));
}

#[test]
fn rider_pr_refuses_an_origin_whose_push_goes_elsewhere_or_to_several_urls() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, _) = given_a_completed_task(&mut world);
    let made = world.git(&world.root, &["init", "-q", "--bare", "other.git"]);
    assert_eq!(made.code, Some(0), "{made:?}");
    let other = world.root.join("other.git");
    let rewrite = format!("url.{}.pushInsteadOf", other.display());
    let other_text = other.to_str().expect("UTF-8 path");
    world.git_ok(&["config", &rewrite, ORIGIN_URL]);
    let rewritten = when_rider_runs(&world, &["pr", &task]);
    world.git_ok(&["config", "--unset", &rewrite]);
    world.git_ok(&["config", "--add", "remote.origin.url", other_text]);
    let doubled = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(rewritten.code, Some(1), "{rewritten:?}");
    assert_eq!(doubled.code, Some(1), "{doubled:?}");
    assert_eq!(origin_refs(&world, &other), Vec::<String>::new());
    assert_eq!(
        origin_refs(&world, &remote.origin),
        vec![format!("refs/heads/{MAIN}")]
    );
    assert_eq!(gh_calls(&remote), Vec::<String>::new());
}

#[test]
fn rider_pr_ignores_a_repo_label_changed_after_completion() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    let other = world.root.join("other/demo.git");
    let made = world.git(&world.root, &["init", "-q", "--bare", "other/demo.git"]);
    assert_eq!(made.code, Some(0), "{made:?}");
    let repo = world.repo.to_str().expect("UTF-8 path");
    let cloned = world.git(&world.root, &["clone", "-q", repo, "clone2"]);
    assert_eq!(cloned.code, Some(0), "{cloned:?}");
    let clone = world.root.join("clone2");
    let tracked = format!("origin/{branch}");
    for args in [
        &["branch", &branch, &tracked][..],
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:other/demo.git",
        ],
    ] {
        let done = world.git(&clone, args);
        assert_eq!(done.code, Some(0), "{args:?}: {done:?}");
    }
    let label = format!("repo={}", clone.display());
    let relabelled = world.vb_as("implement-9", &["label", &task, &label], None);
    assert_eq!(relabelled.code, Some(0), "{relabelled:?}");

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(0), "{output:?}");
    assert_eq!(origin_refs(&world, &other), Vec::<String>::new());
    let refs = vec![format!("refs/heads/{MAIN}"), format!("refs/heads/{branch}")];
    assert_eq!(origin_refs(&world, &remote.origin), refs);
}

#[test]
fn rider_pr_opens_the_draft_against_the_base_the_daemon_used_not_a_relabelled_one() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    let relabelled = world.vb_as("implement-9", &["label", &task, "from=release-1.0"], None);
    assert_eq!(relabelled.code, Some(0), "{relabelled:?}");

    let output = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(output.code, Some(0), "{output:?}");
    let create = create_call(&branch, "Add one work file.", MAIN);
    assert_eq!(gh_calls(&remote), vec![list_call(&branch), create]);
}

#[test]
fn rider_pr_never_runs_an_ssh_command_planted_in_the_repository_config() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let remote = given_an_origin_and_a_fake_gh(&world);
    let (task, branch) = given_a_completed_task(&mut world);
    let ran = world.root.join("planted-ran");
    let planted = world.root.join("planted.sh");
    write_executable(
        &planted,
        &format!("#!/bin/sh\n: > '{}'\nexec ssh \"$@\"\n", ran.display()),
    );
    let planted = planted.to_str().expect("UTF-8 path");
    world.git_ok(&["config", "core.sshCommand", planted]);

    let published = when_rider_runs(&world, &["pr", &task]);

    assert_eq!(published.code, Some(0), "{published:?}");
    let head = branch_head(&world, &branch);
    assert_eq!(origin_head(&world, &remote.origin, &branch), Some(head));
    assert!(!ran.exists(), "rider pr ran the planted ssh command");
}

const PROXY: &str = r#"
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
s.listen()
print(s.getsockname()[1], flush=True)
for _ in range(64):
    c, _ = s.accept()
    c.recv(65536)
    c.sendall(b"HTTP/1.1 407 Proxy Authentication Required\r\n"
              b"Proxy-Authenticate: Basic realm=\"x\"\r\nContent-Length: 0\r\n\r\n")
    c.close()
"#;

#[test]
fn rider_pr_never_runs_a_credential_helper_planted_in_the_repository_config() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let (task, _) = given_a_completed_task(&mut world);
    let mut proxy = std::process::Command::new("python3")
        .args(["-c", PROXY])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("start a proxy that asks for credentials");
    let mut port = String::new();
    let stdout = proxy.stdout.take().expect("the proxy's output");
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut port)
        .expect("read the proxy's port");
    let ran = world.root.join("helper-ran");
    let helper = world.root.join("helper.sh");
    write_executable(&helper, &format!("#!/bin/sh\n: > '{}'\n", ran.display()));
    world.git_ok(&[
        "remote",
        "add",
        "origin",
        "https://github.invalid/acme/demo.git",
    ]);
    world.git_ok(&[
        "config",
        "http.proxy",
        &format!("http://127.0.0.1:{}", port.trim()),
    ]);
    world.git_ok(&[
        "config",
        "credential.helper",
        helper.to_str().expect("UTF-8 path"),
    ]);

    let published = when_rider_runs(&world, &["pr", &task]);

    let _ = proxy.kill();
    let _ = proxy.wait();
    assert_ne!(published.code, Some(0), "{published:?}");
    assert!(!ran.exists(), "rider pr ran the planted credential helper");
}
