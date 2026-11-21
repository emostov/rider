//! The Given, When, and Then steps scenarios are written in.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::fake_agent::{self, Behaviour, Run};
use crate::world::{Daemon, MAIN, PROJECT, World};

/// How long a scenario waits for its tasks to settle.
pub const SETTLE_SECONDS_MAX: u64 = 90;
/// How long SIGTERM may take to end the daemon.
pub const DAEMON_STOP_SECONDS_MAX: u64 = 90;
/// The task states that end an attempt.
pub const SETTLED: [&str; 4] = ["completed", "failed", "blocked", "canceled"];
const POLL_MILLIS: u64 = 200;
const LIST_LIMIT: &str = "500";
const SCHEMAS: [(&str, &str); 2] = [
    (
        "rider-result@2",
        "skills/ride-swarm/schemas/rider-result@2.json",
    ),
    (
        "rider-finding@1",
        "skills/ride-swarm/schemas/rider-finding.json",
    ),
];

/// A task as the lead would create it; `role` is set last, and only when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    pub text: String,
    pub role: Option<String>,
    pub labels: Vec<(String, String)>,
    pub needs: Vec<String>,
}

impl NewTask {
    /// A task with only its text; add a role and labels with the builder methods.
    pub fn new(text: &str) -> NewTask {
        assert!(text.contains(' '), "a brief is a sentence");
        NewTask {
            text: text.to_string(),
            role: None,
            labels: Vec::new(),
            needs: Vec::new(),
        }
    }

    pub fn role(mut self, role: &str) -> NewTask {
        self.role = Some(role.to_string());
        self
    }

    pub fn label(mut self, key: &str, value: &str) -> NewTask {
        assert!(key != "role", "role is set with NewTask::role");
        self.labels.push((key.to_string(), value.to_string()));
        self
    }

    pub fn needs(mut self, id: &str) -> NewTask {
        self.needs.push(id.to_string());
        self
    }
}

/// Given a running board with both schemas, a rider home, and a repository with one commit
/// on `main`; `None` when the Board CLI or Node is not installed.
pub fn given_a_board_and_a_repository() -> Option<World> {
    let mut world = World::new()?;
    world.start_board();
    given_the_schemas_and_a_repository(&world);
    Some(world)
}

/// Given the board answers: Rider's schemas on it, and a scratch repository with one commit.
pub fn given_the_schemas_and_a_repository(world: &World) {
    register_schemas(world);
    fs::create_dir_all(&world.repo).expect("create the repository");
    let repo = world.repo.clone();
    for args in [
        &["init", "-q", "-b", MAIN][..],
        &["config", "user.name", "Scenario Tester"],
        &["config", "user.email", "scenario@example.invalid"],
        &["config", "commit.gpgsign", "false"],
    ] {
        assert_eq!(world.git(&repo, args).code, Some(0), "git {args:?}");
    }
    fs::write(repo.join("README.md"), "scratch\n").expect("write the first file");
    world.git_ok(&["add", "README.md"]);
    world.git_ok(&["commit", "-q", "-m", "Start the scratch repository"]);
    assert_eq!(world.git_ok(&["branch", "--show-current"]), MAIN);
}

fn register_schemas(world: &World) {
    for (name, file) in SCHEMAS {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let file = file.to_str().expect("UTF-8 path");
        world.vb_lead(&["schemas", "create", name, "--file", file], None);
    }
}

/// When the board's data is deleted and a fresh board, with the schemas, takes its place.
pub fn when_the_board_is_wiped(world: &mut World) {
    world.wipe_board();
    register_schemas(world);
}

/// When the board's database is emptied where it lies, and the board, with the schemas, starts
/// again on it.
pub fn when_the_board_is_reinitialised_in_place(world: &mut World) {
    world.reinitialise_board_in_place();
    register_schemas(world);
}

/// When the board stops, keeping its data, and rider is pointed at a second, fresh board, with
/// the schemas, at another URL.
pub fn when_rider_moves_to_another_board(world: &mut World) {
    world.start_another_board();
    register_schemas(world);
}

/// Given the board keeps its data in rider's home, where `vb serve --dir ~/.rider/board` puts it.
pub fn given_the_board_keeps_its_data_in_rider_home(world: &mut World) {
    let dir = world.home.join("board");
    world.move_board_data(dir);
}

/// Given a database in rider's home's board directory that no running board uses.
pub fn given_a_stray_board_database_in_rider_home(world: &World) {
    let dir = world.home.join("board");
    fs::create_dir_all(&dir).expect("create the stray board directory");
    fs::write(dir.join("local.db"), "not this board's data\n").expect("write a stray database");
}

/// Given a worktree on a new `branch` at `id`'s worktree directory that rider did not make.
pub fn given_a_leftover_worktree(world: &World, id: &str, branch: &str) -> PathBuf {
    let dir = worktree_dir(world, id);
    let dir_text = dir.to_str().expect("UTF-8 path");
    world.git_ok(&["worktree", "add", "-q", "-b", branch, dir_text, MAIN]);
    dir
}

/// The worktree's HEAD and its `git status --porcelain`, to show it was left untouched.
pub fn worktree_state(world: &World, dir: &Path) -> (String, String) {
    let head = world.git(dir, &["rev-parse", "HEAD"]);
    let status = world.git(dir, &["status", "--porcelain"]);
    assert_eq!((head.code, status.code), (Some(0), Some(0)));
    (head.stdout, status.stdout)
}

/// The task's whole `vb read <id> --json` output, where its block reason is.
pub fn task_text(world: &World, id: &str) -> String {
    world.vb_lead(&["read", id, "--json"], None)
}

/// An implement task in the world's repository, cut from `main`.
pub fn an_implement_task(world: &World, text: &str) -> NewTask {
    NewTask::new(text)
        .label("repo", world.repo_label())
        .label("from", MAIN)
        .role("implement")
}

/// A qa task in the world's repository that tests the work of task `of` once it is done.
pub fn a_qa_task(world: &World, of: &str, text: &str) -> NewTask {
    NewTask::new(text)
        .label("repo", world.repo_label())
        .label("of", of)
        .needs(of)
        .role("qa")
}

/// Given the fake agent of `task` behaves as `plan[n]` on attempt n, the last repeating.
pub fn given_the_agent_will(world: &World, task: &str, plan: &[Behaviour]) {
    fake_agent::plan(&world.root, task, plan);
}

/// When the lead creates `task`: text and labels first, then `role` on its own.
pub fn when_the_lead_creates(world: &World, task: &NewTask) -> String {
    let channel = format!("work/{PROJECT}");
    let mut args: Vec<String> = ["tasks", "create", &channel, "-", "--json"]
        .map(String::from)
        .into();
    for (key, value) in &task.labels {
        args.extend(["--label".to_string(), format!("{key}={value}")]);
    }
    if !task.needs.is_empty() {
        args.push("--needs".to_string());
        args.extend(task.needs.iter().cloned());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = world.vb_lead(&args, Some(&task.text));
    let created: Created = parse(&output);
    let id = created.task.or(created.message);
    let id = id.unwrap_or_else(|| panic!("no id in {output}")).id;
    if let Some(role) = &task.role {
        world.vb_lead(&["label", &id, &format!("role={role}")], None);
    }
    assert!(!id.is_empty());
    id
}

/// When `tasks` have all settled, within `SETTLE_SECONDS_MAX`; returns their states in order.
pub fn when_the_tasks_settle(world: &World, tasks: &[&str]) -> Vec<String> {
    when_the_tasks_settle_within(world, tasks, SETTLE_SECONDS_MAX)
}

/// When `tasks` have all settled, within `seconds`; returns their states in order.
pub fn when_the_tasks_settle_within(world: &World, tasks: &[&str], seconds: u64) -> Vec<String> {
    assert!(!tasks.is_empty() && seconds >= SETTLE_SECONDS_MAX);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let states: Vec<String> = tasks.iter().map(|id| task_state(world, id)).collect();
        if states.iter().all(|state| SETTLED.contains(&state.as_str())) {
            return states;
        }
        if Instant::now() >= deadline {
            panic!(
                "tasks {tasks:?} were {states:?} after {seconds} seconds; daemon log:\n{}",
                world.daemon_log()
            );
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When the daemon runs until `tasks` settle, each within `SETTLE_SECONDS_MAX` of the one
/// before, and is then sent SIGTERM; returns the states and exit.
pub fn when_the_daemon_runs_until_settled(
    world: &mut World,
    tasks: &[&str],
) -> (Vec<String>, Option<i32>) {
    given_a_running_daemon(world);
    let states: Vec<String> = tasks
        .iter()
        .flat_map(|task| when_the_tasks_settle(world, &[task]))
        .collect();
    when_the_daemon_is_sent_sigterm(world);
    let exit = then_the_daemon_exits(world);
    assert_eq!(states.len(), tasks.len());
    (states, exit)
}

#[derive(Deserialize)]
struct Created {
    task: Option<Id>,
    message: Option<Id>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

/// A task's `vb read <id> --json`, naming the fields scenarios read.
#[derive(Deserialize)]
pub struct TaskRead {
    message: TaskMessage,
    replies: Vec<Value>,
}

#[derive(Deserialize)]
struct TaskMessage {
    labels: BTreeMap<String, String>,
    task: TaskFields,
}

#[derive(Deserialize)]
struct TaskFields {
    state: String,
    epoch: u64,
    claim: Option<Claim>,
    blocked: Option<Blocked>,
}

#[derive(Deserialize)]
struct Blocked {
    kind: String,
    reason: String,
}

#[derive(Deserialize)]
struct Claim {
    until: String,
    actor: String,
}

#[derive(Deserialize)]
struct Reply {
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    schema: String,
    data: Value,
}

#[derive(Deserialize)]
struct Listing {
    tasks: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    message: Id,
}

fn parse<T: DeserializeOwned>(json: &str) -> T {
    serde_json::from_str(json)
        .unwrap_or_else(|error| panic!("not the expected JSON ({error}): {json}"))
}

/// A task's `vb read <id> --json`.
pub fn read_task(world: &World, id: &str) -> TaskRead {
    parse(&world.vb_lead(&["read", id, "--json"], None))
}

/// A blocked task's block kind and reason, or `None` when it is not blocked.
pub fn task_block(world: &World, id: &str) -> Option<(String, String)> {
    let blocked = read_task(world, id).message.task.blocked;
    blocked.map(|blocked| (blocked.kind, blocked.reason))
}

/// A task's state: `ready`, `claimed`, `completed`, `failed`, `blocked`, or `canceled`.
pub fn task_state(world: &World, id: &str) -> String {
    read_task(world, id).message.task.state
}

/// A task's labels.
pub fn task_labels(world: &World, id: &str) -> BTreeMap<String, String> {
    read_task(world, id).message.labels
}

/// One label of a task, or `None` when it has no such label.
pub fn task_label(world: &World, id: &str, key: &str) -> Option<String> {
    task_labels(world, id).remove(key)
}

/// The replies in a task's thread, oldest first, as the board returns them.
pub fn task_replies(world: &World, id: &str) -> Vec<Value> {
    read_task(world, id).replies
}

/// The data of every structured record of `schema` (such as `rider-result`) in a thread.
pub fn task_records(world: &World, id: &str, schema: &str) -> Vec<Value> {
    let replies = task_replies(world, id);
    let count = replies.len();
    let records: Vec<Value> = replies
        .iter()
        .map(|reply| {
            Reply::deserialize(reply).unwrap_or_else(|error| panic!("a reply ({error}): {reply}"))
        })
        .flat_map(|reply| reply.blocks)
        .filter(|block| block.schema == schema)
        .map(|block| block.data)
        .collect();
    assert!(records.len() <= count.max(1) * 8);
    records
}

/// The ids of the project's tasks in `state` (`vb tasks --state`), as the board lists them.
pub fn tasks_in_state(world: &World, state: &str) -> Vec<String> {
    let channel = format!("work/{PROJECT}");
    let args = [
        "tasks", &channel, "--state", state, "--limit", LIST_LIMIT, "--json",
    ];
    let list: Listing = parse(&world.vb_lead(&args, None));
    list.tasks.into_iter().map(|task| task.message.id).collect()
}

/// The repository's local branches, sorted.
pub fn branches(world: &World) -> Vec<String> {
    let text = world.git_ok(&["branch", "--format=%(refname:short)"]);
    let mut branches: Vec<String> = text.lines().map(str::to_string).collect();
    branches.sort();
    branches
}

/// The commit `branch` points at.
pub fn branch_head(world: &World, branch: &str) -> String {
    world.git_ok(&["rev-parse", "--verify", &format!("refs/heads/{branch}")])
}

/// The subject of the commit `branch` points at.
pub fn branch_head_subject(world: &World, branch: &str) -> String {
    world.git_ok(&["log", "-1", "--format=%s", &format!("refs/heads/{branch}")])
}

/// Whether `ancestor` is an ancestor of, or the same commit as, `descendant`.
pub fn is_ancestor(world: &World, ancestor: &str, descendant: &str) -> bool {
    let output = world.git(
        &world.repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    );
    assert!(
        matches!(output.code, Some(0 | 1)),
        "merge-base failed: {output:?}"
    );
    output.code == Some(0)
}

/// The text of `path` at `branch`, or `None` when the branch has no such file.
pub fn file_at(world: &World, branch: &str, path: &str) -> Option<String> {
    let output = world.git(
        &world.repo,
        &["show", &format!("refs/heads/{branch}:{path}")],
    );
    (output.code == Some(0)).then_some(output.stdout)
}

/// Where rider puts a task's worktree: `$RIDER_HOME/worktrees/<repository name>/<id>`.
pub fn worktree_dir(world: &World, id: &str) -> PathBuf {
    let name = world.repo.file_name().expect("the repository has a name");
    world.home.join("worktrees").join(name).join(id)
}

/// Every fake agent start so far.
pub fn agent_runs(world: &World) -> Vec<Run> {
    fake_agent::runs(&world.root)
}

/// The fake agent starts for one task.
pub fn agent_runs_for(world: &World, id: &str) -> Vec<Run> {
    agent_runs(world)
        .into_iter()
        .filter(|run| run.task == id)
        .collect()
}

/// Then the task's branch label names a branch whose head is the agent's commit.
pub fn then_the_branch_has_the_agents_commit(world: &World, id: &str, attempt: u32) -> String {
    let branch = task_label(world, id, "branch").expect("the task has a branch label");
    assert!(branches(world).contains(&branch), "branch {branch} exists");
    let file = file_at(world, &branch, &fake_agent::work_file(id, attempt));
    assert_eq!(file, Some(fake_agent::work_text(id, attempt)));
    assert_eq!(
        branch_head_subject(world, &branch),
        fake_agent::work_subject(id, attempt)
    );
    branch
}

/// Then the task's worktree directory no longer exists.
pub fn then_the_worktree_is_removed(world: &World, id: &str) {
    let dir = worktree_dir(world, id);
    assert!(!dir.exists(), "worktree {} still exists", dir.display());
}

/// Then git lists exactly one worktree, the repository itself.
pub fn then_only_the_repository_is_a_worktree(world: &World) {
    let listed = world.git_ok(&["worktree", "list", "--porcelain"]);
    let worktrees: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .collect();
    assert_eq!(worktrees, vec![world.repo_label()], "git lists {listed}");
}

impl World {
    /// The world's repository as a `repo` label value.
    pub fn repo_label(&self) -> &str {
        self.repo.to_str().expect("UTF-8 path")
    }

    /// What the daemon printed so far.
    pub fn daemon_log(&self) -> String {
        fs::read_to_string(self.root.join("daemon-output.log")).unwrap_or_default()
    }

    /// Lets a fake agent told to `Hold` on `task` go on.
    pub fn release_agent(&self, task: &str) {
        fake_agent::release(&self.root, task);
    }

    /// The most fake agents that ran at the same moment.
    pub fn most_agents_at_once(&self) -> usize {
        fake_agent::most_at_once(&self.root)
    }
}

/// How long a scenario waits for the daemon to say it is following the board.
pub const DAEMON_START_SECONDS_MAX: u64 = 30;
/// The line the daemon prints once it follows the board.
pub const DAEMON_STARTED: &str = "rider daemon: following";

/// The test-only setting that shortens the daemon's lease, attempt limit, tick and kill grace.
pub const TEST_MILLIS_VAR: &str = "RIDER_TEST_MILLIS";
/// The daemon log's line naming a board process the daemon started, before its pid.
pub const BOARD_STARTED: &str = "started the board, pid";
/// The daemon log's line naming a board process a killed daemon started, kept as its own.
pub const BOARD_KEPT: &str = "kept the board started before, pid";
/// The daemon log's line for the first stop signal.
pub const DAEMON_DRAINING: &str = "a stop signal: draining";
/// How long a held `vb` call waits for its release, in tenths of a second.
const HELD_TENTHS_MAX: u32 = 1200;

/// Given a running `rider daemon` that follows the board.
pub fn given_a_running_daemon(world: &mut World) {
    given_a_running_daemon_with(world, &[]);
}

/// Given a running `rider daemon` with `env` added to the world's environment.
pub fn given_a_running_daemon_with(world: &mut World, env: &[(&str, &str)]) {
    when_the_daemon_is_started(world, env);
    let deadline = Instant::now() + Duration::from_secs(DAEMON_START_SECONDS_MAX);
    while !world.daemon_log().contains(DAEMON_STARTED) {
        assert!(
            Instant::now() < deadline,
            "the daemon did not start within {DAEMON_START_SECONDS_MAX} seconds:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When `rider daemon` is spawned with `env` added, not waiting for it to start following.
pub fn when_the_daemon_is_started(world: &mut World, env: &[(&str, &str)]) {
    assert!(world.daemon.is_none(), "one daemon per world");
    let log =
        fs::File::create(world.root.join("daemon-output.log")).expect("create the daemon output");
    let err = log.try_clone().expect("share the daemon output");
    let mut command = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    command.envs(env.iter().copied());
    let child = command
        .arg("daemon")
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn()
        .expect("start rider daemon");
    world.daemon = Some(Daemon { child });
    assert!(world.daemon.is_some());
}

/// When the daemon is sent SIGTERM.
pub fn when_the_daemon_is_sent_sigterm(world: &World) {
    let daemon = world.daemon.as_ref().expect("the daemon was started");
    let status = std::process::Command::new("/bin/kill")
        .args(["-TERM", &daemon.child.id().to_string()])
        .stdin(Stdio::null())
        .status()
        .expect("run kill");
    assert!(status.success());
}

/// When the daemon, once its log shows it is draining, is sent a second SIGTERM; two sent
/// back to back can reach it as one.
pub fn when_the_daemon_is_sent_a_second_sigterm(world: &World) {
    let log = world.home.join("daemon.log");
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !fs::read_to_string(&log)
        .unwrap_or_default()
        .contains(DAEMON_DRAINING)
    {
        assert!(
            Instant::now() < deadline,
            "the daemon never drained:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
    when_the_daemon_is_sent_sigterm(world);
}

/// Then the daemon exits within `DAEMON_STOP_SECONDS_MAX`; returns its exit code.
pub fn then_the_daemon_exits(world: &mut World) -> Option<i32> {
    let daemon = world.daemon.as_mut().expect("the daemon was started");
    let deadline = Instant::now() + Duration::from_secs(DAEMON_STOP_SECONDS_MAX);
    let status = loop {
        if let Some(status) = daemon.child.try_wait().expect("poll the daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit within {DAEMON_STOP_SECONDS_MAX} seconds:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    };
    world.daemon = None;
    status.code()
}

/// Waits up to `seconds` for `task` to leave `ready`; returns the state it was found in.
pub fn when_the_task_leaves_ready_within(world: &World, task: &str, seconds: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let state = task_state(world, task);
        if state != "ready" || Instant::now() >= deadline {
            return state;
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// Waits until a fake agent has started on `task`; returns its run.
pub fn when_the_agent_has_started(world: &World, task: &str) -> Run {
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    loop {
        if let Some(run) = agent_runs_for(world, task).into_iter().next() {
            return run;
        }
        assert!(
            Instant::now() < deadline,
            "no agent started on task {task}:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// Runs `rider <args>` on the world's home to its end.
pub fn when_rider_runs(world: &World, args: &[&str]) -> crate::world::Output {
    let mut command = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    command.args(args);
    crate::world::run_bounded(command, None)
}

/// When `rider status` is run until its output contains `text`, for at most `seconds`.
pub fn when_rider_status_shows_within(world: &World, text: &str, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if when_rider_runs(world, &["status"]).stdout.contains(text) {
            return true;
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
    false
}

/// The command lines of the `vb tail` processes following the world's board as the daemon.
pub fn daemon_feeds(world: &World) -> Vec<String> {
    let mut command = std::process::Command::new("/bin/ps");
    command
        .args(["-A", "-ww", "-o", "command="])
        .stdin(Stdio::null());
    let output = crate::world::run_bounded(command, None);
    assert_eq!(output.code, Some(0), "{output:?}");
    let url = format!("--url {}", world.url);
    let feeds = output.stdout.lines().filter(|line| {
        line.contains(" tail ") && line.contains(&url) && line.contains("--as rider-daemon")
    });
    feeds.map(str::to_string).collect()
}

/// Whether process `pid` still exists.
pub fn is_alive(pid: u32) -> bool {
    let status = std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run kill");
    status.success()
}

/// A task's claim lease end, as the board shows it; `None` when it is not claimed.
pub fn task_claim_until(world: &World, id: &str) -> Option<String> {
    read_task(world, id)
        .message
        .task
        .claim
        .map(|claim| claim.until)
}

/// How many times the task has been claimed, as the board shows it.
pub fn task_epoch(world: &World, id: &str) -> u64 {
    read_task(world, id).message.task.epoch
}

/// The profile holding the task's claim, as the board shows it; `None` when it is not claimed.
pub fn task_claim_profile(world: &World, id: &str) -> Option<String> {
    let claim = read_task(world, id).message.task.claim?;
    // The board names a holder `<owner>+<profile>`.
    let profile = claim
        .actor
        .rsplit_once('+')
        .map(|(_, profile)| profile.to_string());
    Some(profile.unwrap_or(claim.actor))
}

/// When the daemon is sent SIGKILL, as a crash; waits for it to be gone.
pub fn when_the_daemon_crashes(world: &mut World) {
    let daemon = world.daemon.as_ref().expect("the daemon was started");
    let status = std::process::Command::new("/bin/kill")
        .args(["-KILL", &daemon.child.id().to_string()])
        .stdin(Stdio::null())
        .status()
        .expect("run kill");
    assert!(status.success());
    assert_eq!(then_the_daemon_exits(world), None);
}

/// Waits until the daemon's `agents.json` names process group `pid`.
pub fn when_agents_json_names(world: &World, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    let named = format!("\"group\":{pid},");
    loop {
        let text = fs::read_to_string(world.home.join("agents.json")).unwrap_or_default();
        if text.contains(&named) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "agents.json never named {pid}: {text}"
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When `id`'s claim has been renewed past `first`, within `SETTLE_SECONDS_MAX`; returns its end.
pub fn when_the_claim_moves_past(world: &World, id: &str, first: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    loop {
        let until = task_claim_until(world, id);
        let until = until.unwrap_or_else(|| panic!("task {id} is no longer claimed"));
        if until.as_str() > first {
            return until;
        }
        assert!(
            Instant::now() < deadline,
            "the claim on task {id} stayed until {until}:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// Waits up to `seconds` for process `pid` to end; returns whether it did.
pub fn when_the_process_ends_within(pid: u32, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if !is_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// How many fake agents are running now.
pub fn agents_running(world: &World) -> usize {
    fake_agent::running_pids(&world.root).len()
}

/// Given every `vb` run whose arguments contain the word `word` touches `marker` and then
/// waits `seconds` before it runs.
pub fn given_vb_runs_slowly(world: &World, word: &str, marker: &Path, seconds: u32) {
    assert!(!word.is_empty() && !word.contains(['\'', ' ']));
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" {word} "*) : > '{}'; sleep {seconds};; esac
exec '{}' "$@"
"#,
        marker.display(),
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Given every `vb` run whose arguments contain the word `word` touches `marker` and then
/// waits, up to `HELD_TENTHS_MAX` tenths of a second, until `release` exists.
pub fn given_vb_waits_for(world: &World, word: &str, marker: &Path, release: &Path) {
    assert!(!word.is_empty() && !word.contains(['\'', ' ']));
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" {word} "*)
  : > '{}'
  n=0
  while [ ! -e '{}' ] && [ $n -lt {HELD_TENTHS_MAX} ]; do sleep 0.1; n=$((n + 1)); done;;
esac
exec '{}' "$@"
"#,
        marker.display(),
        release.display(),
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Given the first `vb read <task>` of each of `tasks` touches `marker-<task>` under the world's
/// root and then waits, up to `HELD_TENTHS_MAX` tenths of a second, until `release` exists.
pub fn given_vb_holds_the_first_read_of(world: &World, tasks: &[&str], release: &Path) {
    assert!(!tasks.is_empty());
    assert!(
        tasks
            .iter()
            .all(|task| task.bytes().all(|b| b.is_ascii_digit()))
    );
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let root = world.root.display();
    let script = format!(
        r#"#!/bin/sh
case " {} " in *" $2 "*)
  if [ "$1" = read ] && mkdir '{root}/held-'"$2" 2>/dev/null; then
    : > '{root}/marker-'"$2"
    n=0
    while [ ! -e '{}' ] && [ $n -lt {HELD_TENTHS_MAX} ]; do sleep 0.1; n=$((n + 1)); done
  fi;;
esac
exec '{}' "$@"
"#,
        tasks.join(" "),
        release.display(),
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Given every `vb` call that names the board's identity channel fails, and no other.
pub fn given_vb_fails_the_identity_read(world: &World) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" rider/identity "*) echo 'identity unavailable' >&2; exit 1;; esac
exec '{}' "$@"
"#,
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Waits until `path` exists.
pub fn when_the_file_exists(world: &World, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared:\n{}",
            path.display(),
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// Given the first `times` `vb tasks complete` calls each fail, logged as lines in `completes`.
pub fn given_vb_fails_complete(world: &World, times: u32, completes: &Path) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = tasks ] && [ "$2" = complete ]; then
  echo complete >> '{completes}'
  if [ "$(wc -l < '{completes}')" -le {times} ]; then
    echo 'injected complete failure' >&2
    exit 1
  fi
fi
exec '{real}' "$@"
"#,
        completes = completes.display(),
        real = real.display(),
    );
    crate::world::write_executable(&world.vb, &script);
}

/// A second `vb serve` on a port of its own, killed with its group when it drops.
pub struct OtherBoard {
    pub url: String,
    child: Option<std::process::Child>,
}

impl OtherBoard {
    /// Kills the board and waits until its port refuses connections.
    pub fn stop(&mut self) {
        let mut child = self.child.take().expect("the board is stopped once");
        kill_group(child.id());
        child.wait().expect("wait for the board");
        let address = self.url.trim_start_matches("http://");
        let address = address.split('/').next().expect("the URL has an address");
        let deadline = Instant::now() + Duration::from_secs(OTHER_BOARD_SECONDS_MAX);
        while std::net::TcpStream::connect(address).is_ok() {
            assert!(Instant::now() < deadline, "{address} still answers");
            thread::sleep(Duration::from_millis(POLL_MILLIS));
        }
    }
}

impl Drop for OtherBoard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_group(child.id());
            let _ = child.wait();
        }
    }
}

const OTHER_BOARD_SECONDS_MAX: u64 = 30;
const ORPHAN_SECONDS_MAX: u64 = 10;

fn kill_group(pid: u32) {
    assert!(pid > 1, "never signal init or every process");
    let _ = std::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Given another board, named `name` under the world, that answers with one channel.
pub fn given_another_board(world: &World, name: &str) -> OtherBoard {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("read the bound port").port();
    drop(listener);
    assert!(port != 0 && port != 4100, "never the live board's port");
    let url = format!("http://127.0.0.1:{port}/api/board");
    let log = fs::File::create(world.root.join(format!("{name}.log"))).expect("create a log");
    let err = log.try_clone().expect("share the log");
    let child = world
        .command(&world.vb)
        .args([
            "serve",
            "--port",
            &port.to_string(),
            "--no-open",
            "--no-inspect",
        ])
        .arg("--dir")
        .arg(world.root.join(name))
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn()
        .expect("start vb serve");
    let board = OtherBoard {
        url,
        child: Some(child),
    };
    let deadline = Instant::now() + Duration::from_secs(OTHER_BOARD_SECONDS_MAX);
    let create = ["channels", "create", "scratch", "--url", &board.url];
    while world.vb_as(crate::world::LEAD, &create, None).code != Some(0) {
        assert!(Instant::now() < deadline, "the {name} board did not answer");
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
    board
}

/// Orphaned `vb` processes, killed when this drops so a failed scenario leaks none.
#[derive(Default)]
pub struct Orphans(Vec<u32>);

impl Drop for Orphans {
    fn drop(&mut self) {
        for pid in &self.0 {
            if is_alive(*pid) {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// Given a `vb tail` on `url` whose parent has exited, so its parent is now process 1.
pub fn given_an_orphaned_vb_tail(world: &World, orphans: &mut Orphans, url: &str) -> u32 {
    given_an_orphaned_vb(
        world,
        orphans,
        &["tail", "--url", url, "--as", "rider-daemon"],
    )
}

/// Given `vb` run with `args` whose parent has exited, so its parent is now process 1.
pub fn given_an_orphaned_vb(world: &World, orphans: &mut Orphans, args: &[&str]) -> u32 {
    let mut command = world.command(Path::new("/bin/sh"));
    command
        .args([
            "-c",
            r#""$@" </dev/null >/dev/null 2>&1 &
echo $!"#,
            "sh",
        ])
        .arg(&world.vb)
        .args(args);
    let output = crate::world::run_bounded(command, None);
    assert_eq!(output.code, Some(0), "{output:?}");
    let pid: u32 = output.stdout.trim().parse().expect("sh printed the pid");
    orphans.0.push(pid);
    let deadline = Instant::now() + Duration::from_secs(ORPHAN_SECONDS_MAX);
    while parent_of(pid) != Some(1) {
        assert!(Instant::now() < deadline, "{pid} was not orphaned");
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
    pid
}

/// A board URL on a local port that nothing listens on.
pub fn an_unused_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("read the bound port").port();
    drop(listener);
    assert!(port != 0 && port != 4100, "never the live board's port");
    format!("http://127.0.0.1:{port}/api/board")
}

fn parent_of(pid: u32) -> Option<u32> {
    let mut command = std::process::Command::new("/bin/ps");
    command
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stdin(Stdio::null());
    let output = crate::world::run_bounded(command, None);
    output.stdout.trim().parse().ok()
}

/// The line of `output` that names `pid`, with its age replaced by `<age>`.
pub fn line_for_pid(output: &str, pid: u32) -> Option<String> {
    let prefix = format!("pid {pid} age ");
    let line = output.lines().find(|line| line.starts_with(&prefix))?;
    let rest = &line[prefix.len()..];
    let (_, command) = rest.split_once(' ')?;
    Some(format!("{prefix}<age> {command}"))
}

/// Given `text` as the rider home's `rider.toml`.
pub fn given_a_rider_toml(world: &World, text: &str) {
    let path = world.home.join("rider.toml");
    fs::write(&path, text).expect("write rider.toml");
    assert!(path.is_file());
}

/// The model attempt `attempt` of `task`'s agent was started with.
pub fn agent_model(world: &World, task: &str, attempt: u32) -> String {
    let args = fake_agent::args(&world.root, task, attempt);
    let mut lines = args.lines().skip_while(|line| *line != "--model");
    assert_eq!(lines.next(), Some("--model"), "{args}");
    lines.next().expect("a model follows --model").to_string()
}

/// The prompt attempt `attempt` of `task`'s agent was started with: everything after `--`.
pub fn agent_prompt(world: &World, task: &str, attempt: u32) -> String {
    let args = fake_agent::args(&world.root, task, attempt);
    let (_, prompt) = args.split_once("\n--\n").expect("the prompt follows --");
    prompt.to_string()
}

/// The pids of the board processes the daemon started, oldest first, from its log.
pub fn boards_the_daemon_started(world: &World) -> Vec<u32> {
    let log = fs::read_to_string(world.home.join("daemon.log")).unwrap_or_default();
    (log.lines())
        .filter(|line| line.contains(BOARD_STARTED))
        .filter_map(|line| line.rsplit(' ').next()?.parse().ok())
        .collect()
}

/// Waits up to `seconds` until the daemon has started `count` board processes; returns them.
pub fn when_the_daemon_has_started_boards(world: &World, count: usize, seconds: u64) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let boards = boards_the_daemon_started(world);
        if boards.len() >= count {
            return boards;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon started {boards:?}, not {count} boards:\n{}",
            world.daemon_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When the process group led by `pid` is sent SIGKILL.
pub fn when_the_group_is_killed(pid: u32) {
    let status = std::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .status()
        .expect("run kill");
    assert!(status.success(), "killing group {pid} failed: {status}");
}

/// Upper bound on the requests a client that never reads sends before the daemon stops
/// reading them; past every kernel buffer and the daemon's backlog on Linux and macOS.
const FLOOD_REQUESTS_MAX: u64 = 1 << 16;
/// How long a write to the daemon may wait before the daemon is taken to have stopped reading.
const FLOOD_STALL_SECONDS: u64 = 3;
/// How long a client that lagged waits for the daemon to end its connection.
const LAGGED_END_SECONDS_MAX: u64 = 30;
/// Upper bound on the reads taken to reach the end of a connection.
const LAGGED_READS_MAX: usize = 1 << 20;

/// A raw connection to the running daemon's socket.
pub fn given_a_client_on_the_daemon_socket(world: &World) -> UnixStream {
    UnixStream::connect(world.home.join("rider.sock")).expect("connect to the daemon socket")
}

/// When the client sends status requests and never reads, until a write waits
/// `FLOOD_STALL_SECONDS` or is refused because the daemon stopped reading; returns the
/// requests sent whole.
pub fn when_the_client_sends_status_requests_without_reading(client: &mut UnixStream) -> u64 {
    let stall = Duration::from_secs(FLOOD_STALL_SECONDS);
    client
        .set_write_timeout(Some(stall))
        .expect("set a write timeout");
    for id in 1..=FLOOD_REQUESTS_MAX {
        let request = format!("{{\"v\":1,\"id\":{id},\"cmd\":\"status\"}}\n");
        match client.write_all(request.as_bytes()) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock
                        | ErrorKind::TimedOut
                        | ErrorKind::ConnectionReset
                        | ErrorKind::BrokenPipe
                ) =>
            {
                return id - 1;
            }
            Err(error) => panic!("sending request {id} failed: {error}"),
        }
    }
    panic!("the daemon read all {FLOOD_REQUESTS_MAX} requests from a client that never read");
}

/// Then the client reads until the daemon ends its connection; returns every line, each
/// parsed as JSON, failing if a line is cut short or the end is not reached in time.
pub fn then_the_client_reads_to_the_end(mut client: &UnixStream) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(LAGGED_END_SECONDS_MAX);
    let mut data = Vec::new();
    let mut buffer = vec![0; 64 * 1024];
    for _ in 0..LAGGED_READS_MAX {
        let left = deadline.saturating_duration_since(Instant::now());
        let bytes = data.len();
        assert!(!left.is_zero(), "no end of stream in time: {bytes} bytes");
        // macOS refuses the timeout with EINVAL once the daemon has closed its end; a read
        // then returns at once, so the error is safe to ignore.
        let _ = client.set_read_timeout(Some(left));
        match client.read(&mut buffer) {
            Ok(0) => {
                let text = String::from_utf8(data).expect("the daemon sends UTF-8");
                assert!(text.ends_with('\n'), "a line was cut short: {bytes} bytes");
                return text.lines().map(parse::<Value>).collect();
            }
            Ok(read) => data.extend_from_slice(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            // Linux resets the connection when the daemon closes it with the client's
            // requests still unread, which it may do once its bound on reading them is reached.
            Err(error) if error.kind() == ErrorKind::ConnectionReset => {
                let text = String::from_utf8(data).expect("the daemon sends UTF-8");
                assert!(
                    text.ends_with('\n'),
                    "reset after a cut line: {bytes} bytes, {error}"
                );
                let lines: Vec<Value> = text.lines().map(parse::<Value>).collect();
                let code = lines.last().and_then(|line| line["error"]["code"].as_str());
                assert_eq!(code, Some("lagged"), "reset before lagged: {error}");
                return lines;
            }
            Err(error) => panic!("reading to the end failed after {bytes} bytes: {error}"),
        }
    }
    panic!("no end of stream within {LAGGED_READS_MAX} reads");
}

/// How long a scenario waits for `rider wait` to end.
const WAIT_SECONDS_MAX: u64 = 90;
/// The exit code of a `rider wait` that timed out.
pub const WAIT_TIMED_OUT: i32 = 3;

/// When the lead starts `rider wait <project> <args>` in the background.
pub fn when_rider_wait_starts(world: &World, args: &[&str]) -> std::process::Child {
    let mut command = world.command(Path::new(env!("CARGO_BIN_EXE_rider")));
    command
        .args(["wait", PROJECT])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command.spawn().expect("start rider wait")
}

/// Then `rider wait` ends within `WAIT_SECONDS_MAX`; returns its exit code and output.
pub fn then_rider_wait_ends(child: std::process::Child) -> crate::world::Output {
    let (sender, ended) = std::sync::mpsc::sync_channel(1);
    let pid = child.id();
    thread::spawn(move || drop(sender.send(child.wait_with_output())));
    let Ok(output) = ended.recv_timeout(Duration::from_secs(WAIT_SECONDS_MAX)) else {
        kill_group(pid);
        panic!("rider wait did not end within {WAIT_SECONDS_MAX} seconds");
    };
    let output = output.expect("wait for rider wait");
    crate::world::Output {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Given the lead has waited once, so the next wait starts from now.
pub fn given_the_lead_has_waited(world: &World) {
    let output = when_rider_runs(world, &["wait", PROJECT, "--timeout", "0"]);
    assert_eq!(output.code, Some(WAIT_TIMED_OUT), "{output:?}");
}

/// When worker `profile` claims `task`.
pub fn when_a_worker_claims(world: &World, profile: &str, task: &str) {
    let output = world.vb_as(profile, &["tasks", "claim", task, "--lease", "1h"], None);
    assert_eq!(output.code, Some(0), "claim {task}: {output:?}");
}

/// When worker `profile` completes the `task` it holds.
pub fn when_the_worker_completes(world: &World, profile: &str, task: &str) {
    let output = world.vb_as(profile, &["tasks", "complete", task, "done"], None);
    assert_eq!(output.code, Some(0), "complete {task}: {output:?}");
}

/// The `vb tail` processes started by process `parent`.
pub fn feeds_of(parent: u32) -> Vec<u32> {
    let mut command = std::process::Command::new("/bin/ps");
    command
        .args(["-A", "-ww", "-o", "pid=,ppid=,command="])
        .stdin(Stdio::null());
    let output = crate::world::run_bounded(command, None);
    assert_eq!(output.code, Some(0), "{output:?}");
    let rows = output.stdout.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        let pid: u32 = words.next()?.parse().ok()?;
        let ppid: u32 = words.next()?.parse().ok()?;
        let tail = words.any(|word| word == "tail");
        (ppid == parent && tail).then_some(pid)
    });
    rows.collect()
}

/// When `rider wait` has started following the board's feed; returns its `vb tail` pids.
pub fn when_the_wait_follows_the_feed(wait: u32) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(WAIT_SECONDS_MAX);
    loop {
        let feeds = feeds_of(wait);
        if !feeds.is_empty() {
            return feeds;
        }
        assert!(
            Instant::now() < deadline,
            "rider wait never started vb tail"
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When process `pid` is sent `signal`, such as `-TERM`.
pub fn when_the_process_is_sent(pid: u32, signal: &str) {
    let status = std::process::Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .stdin(Stdio::null())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill {signal} {pid}");
}

/// Given the first `vb reply` fails and every later one runs.
pub fn given_vb_fails_the_first_reply(world: &World) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = reply ] && mkdir '{}/first-reply' 2>/dev/null; then
  echo 'injected reply failure' >&2
  exit 1
fi
exec '{}' "$@"
"#,
        world.root.display(),
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Given every `vb` call whose arguments match one of the shell `patterns` fails.
pub fn given_vb_fails_every(world: &World, patterns: &[&str]) {
    assert!(!patterns.is_empty());
    assert!(
        patterns
            .iter()
            .all(|pattern| !pattern.is_empty() && !pattern.contains('\''))
    );
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case "$*" in {}) echo "injected $1 failure" >&2; exit 1;; esac
exec '{}' "$@"
"#,
        patterns.join("|"),
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Given the lead holds and completes `task` itself, so it never gets a `branch` label.
pub fn given_the_lead_completes(world: &World, task: &str) {
    world.vb_lead(&["tasks", "claim", task], None);
    world.vb_lead(&["tasks", "complete", task, "done by hand"], None);
    assert_eq!(task_state(world, task), "completed");
}

/// The names of the log files rider kept for `task`, sorted.
pub fn task_logs(world: &World, task: &str) -> Vec<String> {
    let dir = world.home.join("logs").join(task);
    let entries = fs::read_dir(&dir).expect("read the task's log directory");
    let mut names: Vec<String> = entries
        .map(|entry| {
            entry
                .expect("a log entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Given the first `times` `vb tasks <verb>` calls each fail, logged as lines in `calls`.
pub fn given_vb_fails_the_first(world: &World, verb: &str, times: u32, calls: &Path) {
    assert!(!verb.is_empty() && verb.bytes().all(|b| b.is_ascii_lowercase()));
    assert!(times > 0);
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = tasks ] && [ "$2" = {verb} ]; then
  echo {verb} >> '{calls}'
  if [ "$(wc -l < '{calls}')" -le {times} ]; then
    echo 'injected {verb} failure' >&2
    exit 1
  fi
fi
exec '{real}' "$@"
"#,
        calls = calls.display(),
        real = real.display(),
    );
    crate::world::write_executable(&world.vb, &script);
}

/// Given every `vb read <task>` by an implement worker after its first fails, printing `words`
/// words of error text.
pub fn given_vb_fails_later_reads_of(world: &World, task: &str, words: usize) {
    assert!(!task.is_empty() && task.bytes().all(|b| b.is_ascii_digit()));
    assert!(words > 0);
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let root = world.root.display();
    let text = vec!["injected"; words].join(" ");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" --as implement"*) worker=1;; *) worker=0;; esac
if [ $worker = 1 ] && [ "$1" = read ] && [ "$2" = {task} ] && ! mkdir '{root}/read-{task}' 2>/dev/null; then
  echo '{text}' >&2
  exit 1
fi
exec '{}' "$@"
"#,
        real.display()
    );
    crate::world::write_executable(&world.vb, &script);
}
