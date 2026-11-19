//! The world a scenario runs in: a board, a rider home, a repository, and fake agents.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::fake_agent;

/// The project every scenario's pool runs; its tasks go in the channel `work/<PROJECT>`.
pub const PROJECT: &str = "demo";
/// The profile the tests write tasks as.
pub const LEAD: &str = "lead-fable-5-2";
/// The base branch of the scratch repository.
pub const MAIN: &str = "main";

const COMMAND_SECONDS_MAX: u64 = 30;
const OUTPUT_BYTES_MAX: u64 = 4 << 20;
const BOARD_START_SECONDS_MAX: u64 = 30;
const POLL_MILLIS: u64 = 100;
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const INSTALL_HINT: &str = "install Node 22 and the Board CLI: \
                            npm install --prefix ~/.rider/node @vercel/board";

static WORLDS: AtomicU32 = AtomicU32::new(0);

/// A board, a rider home, and a repository, all removed and stopped when it drops.
pub struct World {
    pub root: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub url: String,
    /// The fake `claude` and `codex` scripts, first on the pool's `PATH`.
    pub agents: PathBuf,
    /// Holds `node`, so the Board CLI runs without the rest of the caller's `PATH`.
    pub tools: PathBuf,
    pub vb: PathBuf,
    pub(crate) board: Option<Child>,
    pub(crate) pool: Option<Pool>,
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

fn find_vb() -> Option<PathBuf> {
    if let Some(found) = find_on_path("vb") {
        return Some(found);
    }
    let home = std::env::var_os("HOME")?;
    let installed = PathBuf::from(home).join(".rider/node/node_modules/.bin/vb");
    installed.is_file().then_some(installed)
}

/// The installed `vb` and `node`, or `None` after saying on standard error which is missing.
pub fn find_tools() -> Option<(PathBuf, PathBuf)> {
    let (vb, node) = (find_vb(), find_on_path("node"));
    match (vb, node) {
        (Some(vb), Some(node)) => Some((vb, node)),
        (vb, _) => {
            let missing = if vb.is_none() {
                "vb (the Board CLI)"
            } else {
                "node"
            };
            // `eprintln!` is captured by the test runner; a skip must be seen.
            let line = format!("skipping scenario: {missing} not found; {INSTALL_HINT}\n");
            let _ = std::io::stderr().write_all(line.as_bytes());
            None
        }
    }
}

fn scratch_root() -> PathBuf {
    let n = WORLDS.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = format!("rider-scenario-{}-{n}-{nanos}", std::process::id());
    let root = std::env::temp_dir().join(name);
    fs::create_dir_all(&root).expect("create the scenario's directory");
    let root = fs::canonicalize(&root).expect("resolve the scenario's directory");
    assert!(root.is_absolute() && root.is_dir());
    root
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("read the bound port").port();
    assert!(port != 0 && port != 4100, "never the live board's port");
    port
}

/// Writes an executable file.
pub fn write_executable(path: &Path, text: &str) {
    fs::write(path, text).expect("write an executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make it executable");
    assert!(path.is_file());
}

impl World {
    /// Lays out the directories and starts nothing; `None` when `vb` or `node` is missing.
    pub fn new() -> Option<World> {
        let (installed_vb, node) = find_tools()?;
        let root = scratch_root();
        let home = root.join("rider-home");
        let vb_dir = home.join("node/node_modules/.bin");
        let (agents, tools) = (root.join("agents"), root.join("tools"));
        for dir in [&vb_dir, &agents, &tools, &root.join("user-home")] {
            fs::create_dir_all(dir).expect("create a scenario directory");
        }
        symlink(&installed_vb, vb_dir.join("vb")).expect("link the Board CLI");
        symlink(&node, tools.join("node")).expect("link node");
        fake_agent::install(&root, &agents);
        let url = format!("http://127.0.0.1:{}/api/board", free_port());
        let world = World {
            repo: root.join("repo"),
            vb: vb_dir.join("vb"),
            root,
            home,
            url,
            agents,
            tools,
            board: None,
            pool: None,
        };
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
            .arg(self.root.join("board"))
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

fn read_all(stream: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(stream) = stream {
            let _ = stream.take(OUTPUT_BYTES_MAX).read_to_string(&mut text);
        }
        text
    })
}

/// Runs `command` to its end within `COMMAND_SECONDS_MAX`, killing it at the limit.
pub fn run_bounded(mut command: Command, input: Option<&str>) -> Output {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("start a command");
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(text.as_bytes())
            .expect("write standard input");
    }
    let (stdout, stderr) = (read_all(child.stdout.take()), read_all(child.stderr.take()));
    let deadline = Instant::now() + Duration::from_secs(COMMAND_SECONDS_MAX);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll a command") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("a command ran longer than {COMMAND_SECONDS_MAX} seconds: {command:?}");
        }
        thread::sleep(Duration::from_millis(POLL_MILLIS / 10));
    };
    let output = Output {
        code: status.code(),
        stdout: stdout.join().expect("read standard output"),
        stderr: stderr.join().expect("read standard error"),
    };
    assert!(output.stdout.len() as u64 <= OUTPUT_BYTES_MAX);
    output
}

fn kill_group(pid: u32) {
    assert!(pid > 1, "never signal init or every process");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut pool) = self.pool.take() {
            kill_group(pool.child.id());
            let _ = pool.child.wait();
        }
        for pid in fake_agent::running_pids(&self.root) {
            kill_group(pid);
        }
        if let Some(mut board) = self.board.take() {
            kill_group(board.id());
            let _ = board.wait();
        }
        if self
            .root
            .starts_with(std::env::temp_dir().canonicalize().unwrap_or_default())
        {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
