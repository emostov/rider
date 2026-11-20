//! The Given, When, and Then steps scenarios are written in.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::fake_agent::{self, Behaviour, Run};
use crate::world::{MAIN, PROJECT, Pool, World};

/// How long a scenario waits for its tasks to settle.
pub const SETTLE_SECONDS_MAX: u64 = 90;
/// How long `stop` may take to end the pool.
pub const POOL_STOP_SECONDS_MAX: u64 = 90;
/// The task states that end an attempt.
pub const SETTLED: [&str; 4] = ["completed", "failed", "blocked", "canceled"];
const POLL_MILLIS: u64 = 200;
const LIST_LIMIT: &str = "500";
const SCHEMAS: [(&str, &str); 2] = [
    (
        "rider-result@1",
        "skills/ride-swarm/schemas/rider-result.json",
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
    for (name, file) in SCHEMAS {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let file = file.to_str().expect("UTF-8 path");
        world.vb_lead(&["schemas", "create", name, "--file", file], None);
    }
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
    Some(world)
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

/// When the pool starts: `rider pool <PROJECT>` with its standard input piped.
pub fn when_the_pool_starts(world: &mut World) {
    assert!(world.pool.is_none(), "one pool per world");
    let log = fs::File::create(world.root.join("pool.log")).expect("create the pool log");
    let err = log.try_clone().expect("share the pool log");
    let mut child = world
        .command(Path::new(env!("CARGO_BIN_EXE_rider")))
        .args(["pool", PROJECT])
        .stdin(Stdio::piped())
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn()
        .expect("start rider pool");
    let stdin = child.stdin.take();
    assert!(stdin.is_some());
    world.pool = Some(Pool { child, stdin });
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
                "tasks {tasks:?} were {states:?} after {seconds} seconds; pool log:\n{}",
                world.pool_log()
            );
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When the pool is told `stop`; returns its exit code once it has exited.
// The pool stays in the world until it has exited, so a panic here still lets Drop kill it.
pub fn when_the_pool_is_stopped(world: &mut World) -> Option<i32> {
    let pool = world.pool.as_mut().expect("the pool was started");
    let mut stdin = pool.stdin.take().expect("the pool's standard input");
    stdin.write_all(b"stop\n").expect("write stop");
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(POOL_STOP_SECONDS_MAX);
    let status = loop {
        if let Some(status) = pool.child.try_wait().expect("poll the pool") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the pool did not stop within {POOL_STOP_SECONDS_MAX} seconds"
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    };
    world.pool = None;
    status.code()
}

/// When the pool runs until `tasks` settle and is then stopped; returns the states and exit.
pub fn when_the_pool_runs_until_settled(
    world: &mut World,
    tasks: &[&str],
) -> (Vec<String>, Option<i32>) {
    when_the_pool_starts(world);
    let states = when_the_tasks_settle(world, tasks);
    let exit = when_the_pool_is_stopped(world);
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
    claim: Option<Claim>,
}

#[derive(Deserialize)]
struct Claim {
    until: String,
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

    /// What the pool printed so far.
    pub fn pool_log(&self) -> String {
        fs::read_to_string(self.root.join("pool.log")).unwrap_or_default()
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

/// Given a running `rider daemon` that follows the board.
pub fn given_a_running_daemon(world: &mut World) {
    given_a_running_daemon_with(world, &[]);
}

/// Given a running `rider daemon` with `env` added to the world's environment.
pub fn given_a_running_daemon_with(world: &mut World, env: &[(&str, &str)]) {
    assert!(world.pool.is_none(), "one daemon per world");
    let log = fs::File::create(world.root.join("pool.log")).expect("create the daemon output");
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
    world.pool = Some(Pool { child, stdin: None });
    let deadline = Instant::now() + Duration::from_secs(DAEMON_START_SECONDS_MAX);
    while !world.pool_log().contains(DAEMON_STARTED) {
        assert!(
            Instant::now() < deadline,
            "the daemon did not start within {DAEMON_START_SECONDS_MAX} seconds:\n{}",
            world.pool_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    }
}

/// When the daemon is sent SIGTERM.
pub fn when_the_daemon_is_sent_sigterm(world: &World) {
    let pool = world.pool.as_ref().expect("the daemon was started");
    let status = std::process::Command::new("/bin/kill")
        .args(["-TERM", &pool.child.id().to_string()])
        .stdin(Stdio::null())
        .status()
        .expect("run kill");
    assert!(status.success());
}

/// Then the daemon exits within `POOL_STOP_SECONDS_MAX`; returns its exit code.
pub fn then_the_daemon_exits(world: &mut World) -> Option<i32> {
    let pool = world.pool.as_mut().expect("the daemon was started");
    let deadline = Instant::now() + Duration::from_secs(POOL_STOP_SECONDS_MAX);
    let status = loop {
        if let Some(status) = pool.child.try_wait().expect("poll the daemon") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit within {POOL_STOP_SECONDS_MAX} seconds:\n{}",
            world.pool_log()
        );
        thread::sleep(Duration::from_millis(POLL_MILLIS));
    };
    world.pool = None;
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
            world.pool_log()
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

/// Given every `vb reply` touches `marker` and then waits `seconds` before it runs.
pub fn given_vb_replies_slowly(world: &World, marker: &Path, seconds: u32) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let script = format!(
        r#"#!/bin/sh
case " $* " in *" reply "*) : > '{}'; sleep {seconds};; esac
exec '{}' "$@"
"#,
        marker.display(),
        real.display()
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

/// Given the lead sets `task`'s role to `role` as a slot of `claimed` first reads it, and the
/// first `vb tasks release` of `task` fails; every release is logged as a line in `releases`.
pub fn given_the_role_changes_after_the_claim_and_one_release_fails(
    world: &World,
    task: &str,
    claimed: &str,
    role: &str,
    releases: &Path,
) {
    let real = fs::read_link(&world.vb).expect("the Board CLI is a link");
    fs::remove_file(&world.vb).expect("remove the Board CLI link");
    let changed = world.root.join("role-changed");
    let failed = world.root.join("release-failed");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = read ] && [ "$2" = '{task}' ] && [ ! -e '{changed}' ]; then
  case " $* " in *" --as {claimed}-"*)
    : > '{changed}'
    '{real}' label '{task}' role={role} --url '{url}' --as '{lead}' > /dev/null || exit 1 ;;
  esac
fi
if [ "$1" = tasks ] && [ "$2" = release ] && [ "$3" = '{task}' ]; then
  echo release >> '{releases}'
  if [ ! -e '{failed}' ]; then
    : > '{failed}'
    echo 'injected release failure' >&2
    exit 1
  fi
fi
exec '{real}' "$@"
"#,
        changed = changed.display(),
        failed = failed.display(),
        releases = releases.display(),
        real = real.display(),
        url = world.url,
        lead = crate::world::LEAD,
    );
    crate::world::write_executable(&world.vb, &script);
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
