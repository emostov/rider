//! The world a scenario runs in: a board, a rider home, a repository, and fake agents.

use std::fs;
use std::io::{Error as IoError, Read, Write, stderr};
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::{Builder, TempDir};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use crate::fake_agent;

/// The project every scenario's pool runs; its tasks go in the channel `work/<PROJECT>`.
pub const PROJECT: &str = "demo";
/// The profile the tests write tasks as.
pub const LEAD: &str = "lead-fable-5-2";
/// The base branch of the scratch repository.
pub const MAIN: &str = "main";

const COMMAND_SECONDS_MAX: u64 = 30;
const OUTPUT_BYTES_MAX: u64 = 4 << 20;
const LISTING_BYTES_MAX: u64 = 64 << 20;
const BOARD_START_SECONDS_MAX: u64 = 30;
const POLL_MILLIS: u64 = 100;
const REAP_SECONDS_MAX: u64 = 5;
const SWEEP_QUIET_POLLS: u32 = 5;
const EVENTS_MAX: usize = 3;
const WORLD_PIDS_MAX: usize = 256;
const PROCESSES_MAX: usize = 65_536;
const DROP_FAILURES_MAX: usize = 4;
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const INSTALL_HINT: &str = "install Node 22 and the Board CLI: \
                            npm install --prefix ~/.rider/node @vercel/board";

/// A board, a rider home, and a repository, all removed and stopped when it drops.
pub struct World {
    pub root: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub url: String,
    /// Where the board started by `start_board` keeps its data.
    pub board_dir: PathBuf,
    /// The fake `claude` and `codex` scripts, first on the pool's `PATH`.
    pub agents: PathBuf,
    /// Holds `node`, so the Board CLI runs without the rest of the caller's `PATH`.
    pub tools: PathBuf,
    pub vb: PathBuf,
    pub(crate) board: Option<Child>,
    pub(crate) pool: Option<Pool>,
    /// Holds `root`; taken and removed last, once every process is dead.
    dir: Option<TempDir>,
}

/// A running `rider pool` and the standard input that stops it.
pub struct Pool {
    pub(crate) child: Child,
    pub(crate) stdin: Option<ChildStdin>,
}

/// What a finished command printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file());
    assert!(found.as_ref().is_none_or(|found| found.ends_with(name)));
    found
}

/// The installed `vb` and `node`, or `None` after saying on standard error which is missing.
pub fn find_tools() -> Option<(PathBuf, PathBuf)> {
    let vb = find_on_path("vb").or_else(|| {
        let home = std::env::var_os("HOME")?;
        let installed = PathBuf::from(home).join(".rider/node/node_modules/.bin/vb");
        installed.is_file().then_some(installed)
    });
    let node = find_on_path("node");
    match (vb, node) {
        (Some(vb), Some(node)) => Some((vb, node)),
        (vb, _) => {
            let missing = if vb.is_none() {
                "vb (the Board CLI)"
            } else {
                "node"
            };
            let line = format!("skipping scenario: {missing} not found; {INSTALL_HINT}\n");
            // CI sets this so a missing tool fails the run instead of passing as a skip.
            assert!(
                std::env::var_os("RIDER_SCENARIOS_REQUIRED").is_none_or(|value| value != "1"),
                "{}",
                line.trim_end()
            );
            // `eprintln!` is captured by the test runner; a skip must be seen.
            let _ = stderr().write_all(line.as_bytes());
            None
        }
    }
}

/// Writes an executable file that may be run at once, even while other threads spawn children.
pub fn write_executable(path: &Path, text: &str) {
    // A write descriptor open in this process is inherited by a child another thread forks and
    // stays open there until that child execs; running the file meanwhile fails with ETXTBSY.
    // So a child of its own writes the file.
    let status = Command::new("/bin/sh")
        .args(["-c", r#"printf %s "$2" > "$1""#, "sh"])
        .arg(path)
        .arg(text)
        .stdin(Stdio::null())
        .status()
        .expect("run sh to write an executable");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make it executable");
    assert!(path.is_file());
}

impl World {
    /// Lays out the directories and starts nothing; `None` when `vb` or `node` is missing.
    pub fn new() -> Option<World> {
        let (installed_vb, node) = find_tools()?;
        let url = free_url();
        let temp = std::env::temp_dir().canonicalize();
        let temp = temp.expect("resolve the temporary directory");
        let dir = Builder::new().prefix("rider-scenario-").tempdir_in(temp);
        let dir = dir.expect("create the scenario's directory");
        let root = dir.path().to_path_buf();
        assert!(root.is_absolute());
        let world = World {
            repo: root.join("repo"),
            vb: root.join("rider-home/node/node_modules/.bin/vb"),
            home: root.join("rider-home"),
            agents: root.join("agents"),
            tools: root.join("tools"),
            board_dir: root.join("board"),
            root,
            url,
            board: None,
            pool: None,
            dir: Some(dir),
        };
        let vb_dir = world.vb.parent().expect("vb has a directory");
        for dir in [
            vb_dir,
            &world.agents,
            &world.tools,
            &world.root.join("user-home"),
        ] {
            fs::create_dir_all(dir).expect("create a scenario directory");
        }
        symlink(&installed_vb, &world.vb).expect("link the Board CLI");
        symlink(&node, world.tools.join("node")).expect("link node");
        fake_agent::install(&world.root, &world.agents);
        assert!(world.vb.starts_with(&world.home));
        Some(world)
    }

    /// The `PATH` every child gets: fake agents first, then the Board CLI, node, the system.
    pub fn child_path(&self) -> String {
        let vb_dir = self.vb.parent().expect("vb has a directory");
        let path = format!(
            "{}:{}:{}:{SYSTEM_PATH}",
            self.agents.display(),
            vb_dir.display(),
            self.tools.display()
        );
        assert!(path.starts_with(self.agents.to_str().expect("UTF-8 path")));
        path
    }

    /// A command with none of the caller's environment, only the world's.
    pub fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", self.child_path())
            .env("HOME", self.root.join("user-home"))
            .env("RIDER_HOME", &self.home)
            .env("BOARD_URL", &self.url)
            .env(fake_agent::WORLD_VAR, &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null());
        command
    }

    /// Starts `vb serve` on the world's port and waits until it answers.
    pub fn start_board(&mut self) {
        assert!(self.board.is_none(), "the board is started once");
        let port = self
            .url
            .split(':')
            .nth(2)
            .and_then(|rest| rest.split('/').next());
        let port = port.expect("the URL holds a port").to_string();
        let log = fs::File::create(self.root.join("board.log")).expect("create the board log");
        let err = log.try_clone().expect("share the board log");
        let child = self
            .command(&self.vb)
            .args([
                "serve",
                "--port",
                &port,
                "--no-open",
                "--no-inspect",
                "--dir",
            ])
            .arg(&self.board_dir)
            .stdout(log)
            .stderr(err)
            .process_group(0)
            .spawn()
            .expect("start vb serve");
        self.board = Some(child);
        let deadline = Instant::now() + Duration::from_secs(BOARD_START_SECONDS_MAX);
        while Instant::now() < deadline {
            if self.vb_as(LEAD, &["whoami", "--json"], None).code == Some(0) {
                return;
            }
            thread::sleep(Duration::from_millis(POLL_MILLIS));
        }
        panic!("the board did not answer within {BOARD_START_SECONDS_MAX} seconds");
    }

    fn stop_board(&mut self) {
        let mut board = self.board.take().expect("the board was started");
        assert!(reap(&mut board), "the board outlived its kill");
    }

    /// Stops the board, deletes its data, and starts a fresh board on the same port.
    pub fn wipe_board(&mut self) {
        self.stop_board();
        fs::remove_dir_all(&self.board_dir).expect("delete the board's data");
        self.start_board();
    }

    /// Stops the board, moves its data to `dir`, and starts it again there on the same port.
    pub fn move_board_data(&mut self, dir: PathBuf) {
        self.stop_board();
        fs::create_dir_all(dir.parent().expect("a data directory has a parent"))
            .expect("create the data directory's parent");
        fs::rename(&self.board_dir, &dir).expect("move the board's data");
        self.board_dir = dir;
        self.start_board();
    }

    /// Stops the board, empties its database file where it is, and starts it again.
    pub fn reinitialise_board_in_place(&mut self) {
        self.stop_board();
        let data = self.board_dir.join("local.db");
        let file = fs::OpenOptions::new().write(true).open(&data);
        let file = file.expect("open the board's database");
        file.set_len(0).expect("empty the board's database");
        for journal in ["local.db-wal", "local.db-shm"] {
            let _ = fs::remove_file(self.board_dir.join(journal));
        }
        self.start_board();
    }

    /// Stops the board, keeping its data, and starts a second, fresh board at another URL.
    pub fn start_another_board(&mut self) {
        self.stop_board();
        self.url = free_url();
        self.board_dir = self.root.join("another-board");
        self.start_board();
    }

    /// Runs `vb <args> --url <world> --as <profile>`, with `input` on standard input.
    pub fn vb_as(&self, profile: &str, args: &[&str], input: Option<&str>) -> Output {
        assert!(
            !args.is_empty() && !args.contains(&"--"),
            "text goes on standard input"
        );
        let mut command = self.command(&self.vb);
        command
            .args(args)
            .args(["--url", &self.url, "--as", profile]);
        run_bounded(command, input)
    }

    /// Runs `vb` as the lead and returns its standard output, panicking when it fails.
    pub fn vb_lead(&self, args: &[&str], input: Option<&str>) -> String {
        let output = self.vb_as(LEAD, args, input);
        assert_eq!(output.code, Some(0), "vb {args:?} failed: {output:?}");
        output.stdout
    }

    /// Runs `git <args>` in `dir` with the world's environment.
    pub fn git(&self, dir: &Path, args: &[&str]) -> Output {
        assert!(dir.is_absolute() && !args.is_empty());
        let mut command = self.command(Path::new("git"));
        command.arg("-C").arg(dir).args(args);
        run_bounded(command, None)
    }

    /// Runs `git <args>` in the scratch repository and returns its trimmed standard output.
    pub fn git_ok(&self, args: &[&str]) -> String {
        let output = self.git(&self.repo, args);
        assert_eq!(output.code, Some(0), "git {args:?} failed: {output:?}");
        output.stdout.trim_end().to_string()
    }
}

/// Why a bounded command produced no `Output`.
#[derive(Debug)]
pub enum RunError {
    Spawn(IoError),
    Wait(IoError),
    /// Writing the command's standard input failed.
    Input(IoError),
    /// Reading the command's standard output or error failed.
    Read(IoError),
    /// The command printed more than `OUTPUT_BYTES_MAX` bytes on one stream.
    OutputTooLong,
    /// The command, or a descendant holding its input or output, outlived `COMMAND_SECONDS_MAX`.
    TimedOut,
    /// The command was still running `REAP_SECONDS_MAX` after it was killed.
    Unkilled,
}

enum Event {
    Stdout(String),
    Stderr(String),
    Written,
    Failed(RunError),
}

fn read_all(stream: impl Read, send: SyncSender<Event>, wrap: fn(String) -> Event, most: u64) {
    let mut bytes = Vec::new();
    let read = stream.take(most + 1).read_to_end(&mut bytes);
    let event = match read {
        Err(error) => Event::Failed(RunError::Read(error)),
        Ok(_) if u64::try_from(bytes.len()).map_or(true, |n| n > most) => {
            Event::Failed(RunError::OutputTooLong)
        }
        Ok(_) => wrap(String::from_utf8_lossy(&bytes).into_owned()),
    };
    // The receiver is gone only once the command has already been judged.
    let _ = send.send(event);
}

/// Runs `command` to its end within `COMMAND_SECONDS_MAX`, panicking when it cannot.
pub fn run_bounded(command: Command, input: Option<&str>) -> Output {
    let shown = format!("{command:?}");
    let why = match run_within(command, input) {
        Ok(output) => return output,
        Err(RunError::Spawn(error)) => format!("could not start: {error}"),
        Err(RunError::Wait(error)) => format!("could not be polled: {error}"),
        Err(RunError::Input(error)) => format!("could not be given its input: {error}"),
        Err(RunError::Read(error)) => format!("could not be read: {error}"),
        Err(RunError::OutputTooLong) => format!("printed over {OUTPUT_BYTES_MAX} bytes"),
        Err(RunError::TimedOut) => format!("outlived {COMMAND_SECONDS_MAX} seconds"),
        Err(RunError::Unkilled) => format!("outlived its kill by {REAP_SECONDS_MAX} seconds"),
    };
    panic!("{shown} {why}");
}

/// Runs `command` in its own process group, feeding `input` and reading both outputs, and
/// kills it and its group when the whole exchange fails or outlives `COMMAND_SECONDS_MAX`.
pub fn run_within(command: Command, input: Option<&str>) -> Result<Output, RunError> {
    let deadline = Instant::now() + Duration::from_secs(COMMAND_SECONDS_MAX);
    run_until(command, input, deadline, OUTPUT_BYTES_MAX)
}

/// Runs `command` as `run_within` does, but until `deadline` and with `most` bytes per output.
fn run_until(
    mut command: Command,
    input: Option<&str>,
    deadline: Instant,
    most: u64,
) -> Result<Output, RunError> {
    assert!(most > 0);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let (send, receive) = sync_channel(EVENTS_MAX);
    let mut events = 0;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        let (text, send) = (text.to_string(), send.clone());
        thread::spawn(move || {
            let event = match stdin.write_all(text.as_bytes()) {
                Ok(()) => Event::Written,
                Err(error) => Event::Failed(RunError::Input(error)),
            };
            drop(stdin);
            let _ = send.send(event);
        });
        events += 1;
    }
    if let Some(stdout) = child.stdout.take() {
        let send = send.clone();
        thread::spawn(move || read_all(stdout, send, Event::Stdout, most));
        events += 1;
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || read_all(stderr, send, Event::Stderr, most));
        events += 1;
    }
    assert!(events <= EVENTS_MAX);
    let finished = finish_within(&mut child, &receive, events, deadline, most);
    match finished {
        Ok(output) => Ok(output),
        Err(_) if !reap(&mut child) => Err(RunError::Unkilled),
        Err(error) => Err(error),
    }
}

fn finish_within(
    child: &mut Child,
    receive: &Receiver<Event>,
    mut events: usize,
    deadline: Instant,
    most: u64,
) -> Result<Output, RunError> {
    let step = Duration::from_millis(POLL_MILLIS / 10);
    let mut output = Output {
        code: None,
        stdout: String::new(),
        stderr: String::new(),
    };
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait().map_err(RunError::Wait)?;
        }
        if let (Some(status), 0) = (status, events) {
            output.code = status.code();
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(RunError::TimedOut);
        }
        if events == 0 {
            thread::sleep(step.min(left));
            continue;
        }
        let event = match receive.recv_timeout(step.min(left)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => panic!("a command's I/O thread vanished"),
        };
        match event {
            Event::Stdout(text) => output.stdout = text,
            Event::Stderr(text) => output.stderr = text,
            Event::Written => {}
            Event::Failed(error) => return Err(error),
        }
        events -= 1;
    }
    let within = |text: &str| u64::try_from(text.len()).is_ok_and(|n| n <= most);
    assert!(within(&output.stdout) && within(&output.stderr));
    Ok(output)
}

fn kill(pid: u32, group: bool) {
    assert!(pid > 1, "never signal init or every process");
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return;
    };
    // The target may already be gone; whoever needs it dead checks that afterwards.
    let _ = if group {
        kill_process_group(pid, Signal::KILL)
    } else {
        kill_process(pid, Signal::KILL)
    };
}

/// Kills `child` and its process group; whether it was reaped within `REAP_SECONDS_MAX`.
/// A board URL on a port that was free a moment ago.
fn free_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("read the bound port").port();
    assert!(port != 0 && port != 4100, "never the live board's port");
    format!("http://127.0.0.1:{port}/api/board")
}

fn reap(child: &mut Child) -> bool {
    kill(child.id(), true);
    // A child that left its group is missed by the group signal.
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(REAP_SECONDS_MAX);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(Duration::from_millis(POLL_MILLIS / 10)),
            Err(_) => return false,
        }
    }
    false
}

/// Records `pid` as one of the world's processes, failing past `WORLD_PIDS_MAX` of them.
fn own(pids: &mut Vec<u32>, pid: u32) -> Result<(), String> {
    if pid <= 1 || pid == std::process::id() {
        return Ok(());
    }
    if pids.len() == WORLD_PIDS_MAX {
        return Err(format!(
            "over {WORLD_PIDS_MAX} of the world's processes run"
        ));
    }
    pids.push(pid);
    assert!(pids.len() <= WORLD_PIDS_MAX);
    Ok(())
}

/// The pids of the processes that carry this world's `WORLD_VAR`: every process the world
/// started and every descendant that kept its environment, whatever its command line.
#[cfg(target_os = "macos")]
fn world_pids(world: &World, deadline: Instant) -> Result<Vec<u32>, String> {
    let mark = format!("{}={}", fake_agent::WORLD_VAR, world.root.display());
    let mut command = Command::new("/bin/ps");
    // `-E` appends each process's environment, as `NAME=value` words, to its command line.
    command
        .args(["-E", "-A", "-ww", "-o", "pid=,command="])
        .stdin(Stdio::null());
    let output = run_until(command, None, deadline, LISTING_BYTES_MAX)
        .map_err(|error| format!("ps failed: {error:?}"))?;
    if output.code != Some(0) {
        return Err(format!("ps exited {:?}: {}", output.code, output.stderr));
    }
    let mut pids = Vec::new();
    for (index, line) in output.stdout.lines().enumerate() {
        if index >= PROCESSES_MAX {
            return Err(format!("ps listed over {PROCESSES_MAX} processes"));
        }
        let line = line.trim_start();
        let (pid, rest) = line.split_once(' ').unwrap_or((line, ""));
        let pid: u32 = pid
            .parse()
            .map_err(|_| format!("unrecognised ps line {line:?}"))?;
        if rest.split(' ').any(|word| word == mark) {
            own(&mut pids, pid)?;
        }
    }
    Ok(pids)
}

/// The pids of the processes that carry this world's `WORLD_VAR`: every process the world
/// started and every descendant that kept its environment, whatever its command line.
#[cfg(target_os = "linux")]
fn world_pids(world: &World, deadline: Instant) -> Result<Vec<u32>, String> {
    let mark = format!("{}={}", fake_agent::WORLD_VAR, world.root.display());
    let entries = fs::read_dir("/proc").map_err(|error| format!("reading /proc: {error}"))?;
    let mut pids = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= PROCESSES_MAX {
            return Err(format!("/proc lists over {PROCESSES_MAX} entries"));
        }
        if Instant::now() >= deadline {
            return Err(format!("listing /proc outlived {REAP_SECONDS_MAX} seconds"));
        }
        let entry = entry.map_err(|error| format!("reading /proc: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        // A process that ended, or that another user owns, is not the world's.
        let Ok(file) = fs::File::open(entry.path().join("environ")) else {
            continue;
        };
        let mut environ = Vec::new();
        if file
            .take(LISTING_BYTES_MAX)
            .read_to_end(&mut environ)
            .is_err()
        {
            continue;
        }
        if environ
            .split(|&byte| byte == 0)
            .any(|word| word == mark.as_bytes())
        {
            own(&mut pids, pid)?;
        }
    }
    Ok(pids)
}

/// Kills every process of the world and its group until none is left within `REAP_SECONDS_MAX`.
fn sweep(world: &World) -> Result<(), String> {
    // Rider starts each feed and board call in a process group of its own, so killing the
    // pool's group misses them, and they outlive a pool that exited or was killed. A process
    // forking while it is listed leaves a child that listing missed, so the world is clean
    // only once `SWEEP_QUIET_POLLS` listings in a row find nothing.
    let deadline = Instant::now() + Duration::from_secs(REAP_SECONDS_MAX);
    let mut quiet = 0;
    let mut last = Vec::new();
    while quiet < SWEEP_QUIET_POLLS {
        if Instant::now() >= deadline {
            return Err(format!(
                "the world's processes {last:?} outlived {REAP_SECONDS_MAX} seconds of sweeping"
            ));
        }
        last = world_pids(world, deadline)?;
        quiet = if last.is_empty() { quiet + 1 } else { 0 };
        for &pid in &last {
            kill(pid, true);
            kill(pid, false);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(POLL_MILLIS).min(left));
    }
    assert_eq!(quiet, SWEEP_QUIET_POLLS);
    Ok(())
}

impl Drop for World {
    fn drop(&mut self) {
        let mut failures: Vec<String> = Vec::with_capacity(DROP_FAILURES_MAX);
        if let Some(mut pool) = self.pool.take()
            && !reap(&mut pool.child)
        {
            failures.push("the pool outlived its kill".to_string());
        }
        // After the pool is dead no new agent can start, so one sweep finds them all.
        for pid in fake_agent::running_pids(&self.root) {
            kill(pid, true);
        }
        if let Some(mut board) = self.board.take()
            && !reap(&mut board)
        {
            failures.push("the board outlived its kill".to_string());
        }
        // A live process can still write into the directory, so all die before it is removed.
        if let Err(why) = sweep(self) {
            failures.push(why);
        }
        if let Some(dir) = self.dir.take()
            && let Err(error) = dir.close()
        {
            failures.push(format!("removing {}: {error}", self.root.display()));
        }
        assert!(failures.len() <= DROP_FAILURES_MAX);
        if failures.is_empty() {
            return;
        }
        let why = format!("the scenario world was not cleaned up: {failures:?}\n");
        if thread::panicking() {
            let _ = stderr().write_all(why.as_bytes());
        } else {
            panic!("{why}");
        }
    }
}
