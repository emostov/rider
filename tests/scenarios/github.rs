use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::fake_agent::{self, Behaviour};
use crate::steps::*;
use crate::world::{World, write_executable};

const FAST_POLL: (&str, &str) = ("RIDER_TEST_MILLIS", "tick=20");
const RECORD_SECONDS_MAX: u64 = 30;
const QUIET_MILLIS: u64 = 3000;
const FIX_CYCLE_SECONDS_MAX: u64 = 180;
const STATES: [&str; 7] = [
    "waiting",
    "ready",
    "claimed",
    "completed",
    "failed",
    "blocked",
    "canceled",
];
const HEAD: &str = "2ae54426992ec8249006b5ea9b5531e8900062f3";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/github")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn given_gh_prints(world: &World, pr_list: &str, comments: &str) {
    fs::write(world.root.join("gh-pr.json"), fixture(pr_list)).expect("write the pr fixture");
    fs::write(world.root.join("gh-comments.json"), fixture(comments)).expect("write comments");
    let log = fixture("run-log-failed.txt");
    fs::write(world.root.join("gh-log.txt"), log).expect("write the log fixture");
    given_gh_grants(world, "reviewer", "permission-write.json");
    let root = world.root.display();
    let script = format!(
        r#"#!/bin/sh
echo "$*" >> '{root}/gh-calls.log'
case "$1 $2" in
  pr*) cat '{root}/gh-pr.json' ;;
  api*/permission)
    login=$(basename "$(dirname "$2")")
    cat "{root}/gh-permission-$login.json" 2>/dev/null || {{ echo 'HTTP 404' >&2; exit 1; }} ;;
  api*) cat '{root}/gh-comments.json' ;;
  run*) cat '{root}/gh-log.txt' ;;
  *) exit 1 ;;
esac
"#
    );
    write_executable(&world.agents.join("gh"), &script);
}

fn given_gh_grants(world: &World, login: &str, permission: &str) {
    let path = world.root.join(format!("gh-permission-{login}.json"));
    fs::write(path, fixture(permission)).expect("write the permission fixture");
}

fn gh_calls(world: &World) -> usize {
    fs::read_to_string(world.root.join("gh-calls.log")).map_or(0, |log| log.lines().count())
}

fn given_a_completed_task(world: &mut World) -> String {
    given_a_settled_task(world, Behaviour::Commit, "completed")
}

fn given_a_settled_task(world: &mut World, behaviour: Behaviour, state: &str) -> String {
    let task = when_the_lead_creates(world, &an_implement_task(world, "Add one work file."));
    given_the_agent_will(world, &task, &[behaviour]);
    given_a_running_daemon_with(world, &[FAST_POLL]);
    assert_eq!(when_the_tasks_settle(world, &[&task]), vec![state]);
    task
}

fn queued_fix(record: &Value) -> String {
    let task = record["task"].as_u64();
    task.unwrap_or_else(|| panic!("no fix task in {record}"))
        .to_string()
}

fn then_the_fix_resumes_one_of(world: &World, fix: &str, owners: &[&str]) {
    let onto = task_label(world, fix, "onto").unwrap_or_default();
    assert!(
        owners.contains(&onto.as_str()) && onto != fix,
        "{onto} not in {owners:?}"
    );
    assert_eq!(task_label(world, fix, "resume").as_deref(), Some("1"));
}

fn then_the_fix_resumes(world: &World, fix: &str, owner: &str) {
    let labels = task_labels(world, fix);
    let wanted = [("onto", owner), ("resume", "1"), ("role", "implement")];
    for (key, value) in wanted {
        assert_eq!(
            labels.get(key).map(String::as_str),
            Some(value),
            "{labels:?}"
        );
    }
}

fn then_no_fix_was_queued(world: &World, owner: &str) {
    for state in ["ready", "claimed", "completed", "failed", "blocked"] {
        let tasks = tasks_in_state(world, state);
        let fixes: Vec<&String> = tasks
            .iter()
            .filter(|task| task_label(world, task, "onto").as_deref() == Some(owner))
            .collect();
        assert_eq!(fixes, Vec::<&String>::new(), "{state}");
    }
}

fn project_records(world: &World) -> Vec<Value> {
    let mut tasks: Vec<String> = STATES
        .iter()
        .flat_map(|state| tasks_in_state(world, state))
        .collect();
    tasks.sort_by_key(|task| task.parse::<u64>().expect("a numeric id"));
    tasks.dedup();
    tasks
        .iter()
        .flat_map(|task| task_records(world, task, "rider-github"))
        .collect()
}

fn when_project_records_reach(world: &World, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(FIX_CYCLE_SECONDS_MAX);
    let mut records = project_records(world);
    while records.len() < count {
        assert!(
            Instant::now() < deadline,
            "{records:?}\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(500));
        records = project_records(world);
    }
    thread::sleep(Duration::from_millis(QUIET_MILLIS));
    project_records(world)
}

fn record_with<'a>(records: &'a [Value], key: &str, value: &str) -> &'a Value {
    let found = records.iter().find(|record| record[key] == value);
    found.unwrap_or_else(|| panic!("no record with {key} {value} in {records:?}"))
}

fn when_records_reach(world: &World, task: &str, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECONDS_MAX);
    loop {
        let records = task_records(world, task, "rider-github");
        if records.len() >= count || Instant::now() >= deadline {
            thread::sleep(Duration::from_millis(QUIET_MILLIS));
            return task_records(world, task, "rider-github");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn when_gh_is_called_again(world: &World) {
    when_gh_is_called_past(world, gh_calls(world));
}

fn when_gh_is_called_past(world: &World, before: usize) {
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECONDS_MAX);
    while gh_calls(world) <= before {
        assert!(Instant::now() < deadline, "{}", world.daemon_log());
        thread::sleep(Duration::from_millis(100));
    }
}

const JOB_URL: &str = "https://github.com/example/repo/actions/runs/44868640008/job/136737934594";

fn failed_record(route: &str) -> Value {
    json!({"kind": "checks", "pr": 7, "head": HEAD, "state": "failed", "job": "test",
        "url": JOB_URL, "route": route})
}

#[test]
fn ci_pending_then_failed_posts_one_record_and_a_restart_posts_it_not_again() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));
    given_a_running_daemon_with(&mut world, &[FAST_POLL, ("GH_TOKEN", "a-token")]);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
    when_gh_is_called_again(&world);
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        Vec::<Value>::new()
    );

    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    let records = when_records_reach(&world, &task, 1);
    let fix = queued_fix(&records[0]);
    let mut wanted = failed_record("owner");
    wanted["task"] = json!(fix.parse::<u64>().expect("an id"));
    assert_eq!(records, vec![wanted.clone()]);
    then_the_fix_resumes(&world, &fix, &task);
    let brief = task_text(&world, &fix);
    for part in [
        JOB_URL,
        "job `test`",
        "Process completed with exit code 101.",
    ] {
        assert!(brief.contains(part), "{part} in {brief}");
    }
    assert_eq!(when_the_tasks_settle(&world, &[&fix]), vec!["completed"]);
    let owner_env = fake_agent::gh_env(&world.root, &task, 0);
    assert_eq!(owner_env[..2], ["GH_TOKEN=set", "GITHUB_TOKEN=unset"]);
    let scratch = world.home.join("logs").join(&fix).join("gh-config");
    let fix_env = vec![
        "GH_TOKEN=empty".to_string(),
        "GITHUB_TOKEN=empty".to_string(),
        format!("GH_CONFIG_DIR={}", scratch.display()),
    ];
    assert_eq!(fake_agent::gh_env(&world.root, &fix, 0), fix_env);
    let options = fake_agent::options(&world.root, &fix, 0);
    let resumed = options.windows(2).find(|pair| pair[0] == "--resume");
    let session = fake_agent::session(&task, 0);
    assert_eq!(
        resumed,
        Some(&["--resume".to_string(), session][..]),
        "{options:?}"
    );

    when_the_daemon_is_sent_sigterm(&world);
    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    given_a_running_daemon_with(&mut world, &[FAST_POLL]);
    when_gh_is_called_again(&world);
    when_gh_is_called_again(&world);
    assert_eq!(task_records(&world, &task, "rider-github"), vec![wanted]);
    assert_eq!(
        task_records(&world, &fix, "rider-github"),
        Vec::<Value>::new()
    );
    assert_eq!(tasks_in_state(&world, "ready"), Vec::<String>::new());
}

#[test]
fn a_change_request_queues_a_fix_and_a_question_goes_to_the_lead() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    let task = given_a_completed_task(&mut world);
    given_gh_prints(&world, "pr-list-review.json", "pulls-comments.json");

    let records = when_project_records_reach(&world, 3);

    let url = "https://github.com/example/repo/pull/7";
    let review = record_with(&records, "url", url);
    let split = queued_fix(review);
    let inline = record_with(
        &records,
        "url",
        "https://github.com/example/repo/pull/7#discussion_r1",
    );
    let fix = queued_fix(inline);
    let id = |task: &str| task.parse::<u64>().expect("an id");
    let mut wanted = vec![
        json!({"kind": "review", "pr": 7, "head": HEAD, "state": "COMMENTED",
            "author": "reviewer", "excerpt": "Please split the parser out of the loop.",
            "url": url, "route": "owner", "task": id(&split)}),
        json!({"kind": "comment", "pr": 7, "head": HEAD, "author": "commenter",
            "excerpt": "Is the cap of 64 enough for a large swarm?",
            "url": format!("{url}#issuecomment-1"), "route": "lead"}),
        json!({"kind": "review", "pr": 7, "head": HEAD, "author": "reviewer",
            "file": "src/github.rs", "line": 30, "excerpt": "This loop needs a bound.",
            "url": format!("{url}#discussion_r1"), "route": "owner", "task": id(&fix)}),
    ];
    let mut records = records;
    let by_url = |record: &Value| record["url"].as_str().unwrap_or_default().to_string();
    records.sort_by_key(by_url);
    wanted.sort_by_key(by_url);
    assert_eq!(records, wanted);
    let owners = [task.as_str(), split.as_str()];
    then_the_fix_resumes_one_of(&world, &split, &owners);
    then_the_fix_resumes_one_of(&world, &fix, &owners);
    let read: Value = serde_json::from_str(&task_text(&world, &fix)).expect("a task as JSON");
    let brief = read["message"]["text"].as_str().unwrap_or_default();
    for part in [
        "----- untrusted GitHub comment begins -----\n> This loop needs a bound.\n----- \
         untrusted GitHub comment ends -----",
        "src/github.rs:30",
        "#discussion_r1",
    ] {
        assert!(brief.contains(part), "{part} in {brief}");
    }
}

#[test]
fn a_change_request_from_an_author_without_write_access_only_reaches_the_lead() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-review.json", "pulls-comments.json");
    given_gh_grants(&world, "reviewer", "permission-read.json");
    let task = given_a_completed_task(&mut world);

    let records = when_records_reach(&world, &task, 3);

    let routes: Vec<&Value> = records.iter().map(|record| &record["route"]).collect();
    assert_eq!(routes, vec!["lead", "lead", "lead"]);
    then_no_fix_was_queued(&world, &task);
}

#[test]
fn a_pr_from_a_fork_with_the_same_branch_name_is_not_the_tasks_pr() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-fork.json", "pulls-comments.json");
    let task = given_a_completed_task(&mut world);

    when_gh_is_called_past(&world, 0);
    when_gh_is_called_again(&world);

    assert_eq!(
        task_records(&world, &task, "rider-github"),
        Vec::<Value>::new()
    );
    then_no_fix_was_queued(&world, &task);
}

#[test]
fn a_pr_is_reported_to_the_lead_while_its_task_still_works() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-review.json", "pulls-comments-empty.json");
    let task = when_the_lead_creates(&world, &an_implement_task(&world, "Add one work file."));
    given_the_agent_will(&world, &task, &[Behaviour::Hold]);
    given_a_running_daemon_with(&mut world, &[FAST_POLL]);
    when_the_agent_has_started(&world, &task);

    let records = when_records_reach(&world, &task, 1);

    let url = "https://github.com/example/repo/pull/7#issuecomment-1";
    assert_eq!(
        records,
        vec![
            json!({"kind": "comment", "pr": 7, "head": HEAD, "author": "commenter",
            "excerpt": "Is the cap of 64 enough for a large swarm?", "url": url,
            "route": "lead"})
        ]
    );
    fake_agent::release(&world.root, &task);
    assert_eq!(when_the_tasks_settle(&world, &[&task]), vec!["completed"]);
}

#[test]
fn a_pr_whose_task_has_no_session_goes_to_the_lead() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    let task = given_a_settled_task(&mut world, Behaviour::CommitThenFail, "failed");

    let records = when_records_reach(&world, &task, 1);

    assert_eq!(records, vec![failed_record("lead")]);
    then_no_fix_was_queued(&world, &task);
}

#[test]
fn fix_rounds_past_the_cap_on_one_head_go_to_the_lead() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-three.json");
    let task = given_a_completed_task(&mut world);

    let records = when_project_records_reach(&world, 3);

    let mut routes: Vec<&str> = records
        .iter()
        .map(|record| record["route"].as_str().unwrap_or_default())
        .collect();
    routes.sort_unstable();
    assert_eq!(routes, vec!["lead", "owner", "owner"]);
    let fixes: Vec<String> = records
        .iter()
        .filter(|record| record["route"] == "owner")
        .map(queued_fix)
        .collect();
    let owners = [task.as_str(), fixes[0].as_str(), fixes[1].as_str()];
    then_the_fix_resumes_one_of(&world, &fixes[0], &owners);
    then_the_fix_resumes_one_of(&world, &fixes[1], &owners);
    let fixes = [fixes[0].as_str(), fixes[1].as_str()];
    assert_eq!(when_the_tasks_settle(&world, &fixes), vec!["completed"; 2]);
}

#[test]
fn a_merged_pr_ends_the_watch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-merged.json", "pulls-comments-empty.json");
    let task = given_a_completed_task(&mut world);
    when_gh_is_called_past(&world, 0);
    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    let calls = gh_calls(&world);

    thread::sleep(Duration::from_millis(QUIET_MILLIS));

    assert_eq!(gh_calls(&world), calls);
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        Vec::<Value>::new()
    );
}

#[test]
fn stopping_the_daemon_ends_a_gh_call_in_flight() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let task = given_a_completed_task(&mut world);
    let pid_file = world.root.join("gh-pid");
    let script = format!(
        "#!/bin/sh\necho $$ > '{}'\nexec sleep 20\n",
        pid_file.display()
    );
    write_executable(&world.agents.join("gh"), &script);
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECONDS_MAX);
    let pid = loop {
        let text = fs::read_to_string(&pid_file).unwrap_or_default();
        if let Ok(pid) = text.trim().parse::<u32>() {
            break pid;
        }
        assert!(Instant::now() < deadline, "gh never ran: {task}");
        thread::sleep(Duration::from_millis(100));
    };

    when_the_daemon_is_sent_sigterm(&world);

    assert_eq!(then_the_daemon_exits(&mut world), Some(0));
    assert!(!is_alive(pid), "gh {pid} outlived the daemon");
}

#[test]
fn a_github_off_label_on_the_task_ends_the_watch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let task = given_a_completed_task(&mut world);
    when_gh_is_called_past(&world, 0);

    world.vb_lead(&["label", &task, "github=off"], None);
    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    thread::sleep(Duration::from_millis(QUIET_MILLIS));
    let calls = gh_calls(&world);
    thread::sleep(Duration::from_millis(QUIET_MILLIS));

    assert_eq!(gh_calls(&world), calls);
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        Vec::<Value>::new()
    );
}

#[test]
fn a_ci_failure_whose_owner_session_is_malformed_goes_to_the_lead() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let task = given_a_completed_task(&mut world);
    let session = world.home.join("logs").join(&task).join("session");
    fs::write(&session, "not a session\n").expect("overwrite the recorded session");

    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    let records = when_records_reach(&world, &task, 1);

    assert_eq!(records, vec![failed_record("lead")]);
    then_no_fix_was_queued(&world, &task);
}

#[test]
fn a_record_whose_post_reply_is_lost_is_posted_once() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let task = given_a_settled_task(&mut world, Behaviour::CommitThenFail, "failed");
    let marker = world.root.join("lost-post");
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" rider-github@1 "*)
  if [ ! -e '{marker}' ]; then : > '{marker}'; '{real}' "$@" > /dev/null; exit 1; fi;;
esac
exec '{real}' "$@"
"#,
        marker = marker.display(),
        real = real.display()
    );
    write_executable(&world.vb, &script);

    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    when_records_reach(&world, &task, 1);
    when_gh_is_called_again(&world);
    when_gh_is_called_again(&world);

    assert!(marker.exists());
    assert_eq!(
        task_records(&world, &task, "rider-github"),
        vec![failed_record("lead")]
    );
}

#[test]
fn the_owner_is_the_task_that_last_worked_on_the_branch() {
    let Some(mut world) = given_a_board_and_a_repository() else {
        return;
    };
    given_gh_prints(&world, "pr-list-pending.json", "pulls-comments-empty.json");
    let base = given_a_completed_task(&mut world);
    let older = when_the_lead_creates(&world, &NewTask::new("Fix the work last."));
    let newer = NewTask::new("Fix the work first.")
        .label("repo", world.repo_label())
        .label("onto", &base)
        .role("implement");
    let newer = when_the_lead_creates(&world, &newer);
    assert_eq!(when_the_tasks_settle(&world, &[&newer]), vec!["completed"]);
    world.vb_lead(
        &["label", &older, &format!("repo={}", world.repo_label())],
        None,
    );
    world.vb_lead(&["label", &older, &format!("onto={newer}")], None);
    world.vb_lead(&["label", &older, "role=implement"], None);
    assert_eq!(when_the_tasks_settle(&world, &[&older]), vec!["completed"]);

    given_gh_prints(&world, "pr-list-failed.json", "pulls-comments-empty.json");
    let records = when_records_reach(&world, &older, 1);

    let fix = queued_fix(&records[0]);
    then_the_fix_resumes(&world, &fix, &older);
    assert_eq!(
        task_records(&world, &newer, "rider-github"),
        Vec::<Value>::new()
    );
}
