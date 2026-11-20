//! The Given, When, and Then steps scenarios are written in.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

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
    let created: Value = parse(&world.vb_lead(&args, Some(&task.text)));
    let id = created
        .pointer("/task/id")
        .or_else(|| created.pointer("/message/id"));
    let id = id
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no id in {created}"));
    let id = id.to_string();
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
    let deadline = Instant::now() + Duration::from_secs(SETTLE_SECONDS_MAX);
    loop {
        let states: Vec<String> = tasks.iter().map(|id| task_state(world, id)).collect();
        if states.iter().all(|state| SETTLED.contains(&state.as_str())) {
            return states;
        }
        if Instant::now() >= deadline {
            panic!(
                "tasks {tasks:?} were {states:?} after {SETTLE_SECONDS_MAX} seconds; pool log:\n{}",
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

fn parse(json: &str) -> Value {
    serde_json::from_str(json).unwrap_or_else(|error| panic!("not JSON ({error}): {json}"))
}

/// A task's `vb read <id> --json`: `{message, replies, cursor, schedules}`.
pub fn read_task(world: &World, id: &str) -> Value {
    parse(&world.vb_lead(&["read", id, "--json"], None))
}

/// A task's state: `ready`, `claimed`, `completed`, `failed`, `blocked`, or `canceled`.
pub fn task_state(world: &World, id: &str) -> String {
    let read = read_task(world, id);
    let state = read.pointer("/message/task/state").and_then(Value::as_str);
    state
        .unwrap_or_else(|| panic!("task {id} has no state: {read}"))
        .to_string()
}

/// A task's labels.
pub fn task_labels(world: &World, id: &str) -> BTreeMap<String, String> {
    let read = read_task(world, id);
    let labels = read.pointer("/message/labels").and_then(Value::as_object);
    let labels = labels.unwrap_or_else(|| panic!("task {id} has no labels: {read}"));
    labels
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
        .collect()
}

/// One label of a task, or `None` when it has no such label.
pub fn task_label(world: &World, id: &str, key: &str) -> Option<String> {
    task_labels(world, id).remove(key)
}

/// The replies in a task's thread, oldest first, as the board returns them.
pub fn task_replies(world: &World, id: &str) -> Vec<Value> {
    let read = read_task(world, id);
    let replies = read.pointer("/replies").and_then(Value::as_array);
    replies
        .unwrap_or_else(|| panic!("task {id} has no replies list: {read}"))
        .clone()
}

/// The data of every structured record of `schema` (such as `rider-result`) in a thread.
pub fn task_records(world: &World, id: &str, schema: &str) -> Vec<Value> {
    let replies = task_replies(world, id);
    let blocks = replies
        .iter()
        .filter_map(|reply| reply.pointer("/blocks")?.as_array());
    let records: Vec<Value> = blocks
        .flatten()
        .filter(|block| block.pointer("/schema").and_then(Value::as_str) == Some(schema))
        .filter_map(|block| block.pointer("/data").cloned())
        .collect();
    assert!(records.len() <= replies.len().max(1) * 8);
    records
}

/// The ids of the project's tasks in `state` (`vb tasks --state`), as the board lists them.
pub fn tasks_in_state(world: &World, state: &str) -> Vec<String> {
    let channel = format!("work/{PROJECT}");
    let args = [
        "tasks", &channel, "--state", state, "--limit", LIST_LIMIT, "--json",
    ];
    let list = parse(&world.vb_lead(&args, None));
    let tasks = list.pointer("/tasks").and_then(Value::as_array);
    let tasks = tasks.unwrap_or_else(|| panic!("no tasks list: {list}"));
    tasks
        .iter()
        .map(|task| task.pointer("/message/id").and_then(Value::as_str))
        .map(|id| id.expect("a listed task has an id").to_string())
        .collect()
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
