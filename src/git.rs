//! Thin wrappers over the `git` command-line program, one process per call.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::process::{Pid, Signal, kill_process_group};

use crate::board;

/// Upper bound on what one `git` run may print to stdout, and separately to stderr.
pub const OUTPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Upper bound on the length of a ref name rider passes to `git`.
pub const REF_BYTES_MAX: usize = 255;
/// Upper bound on the worktrees of one repository that rider reads.
pub const WORKTREES_MAX: usize = 256;
const GIT_DEADLINE: Duration = Duration::from_secs(300);
const POLL_MILLIS: u64 = 2;
const POLL_INTERVAL: Duration = Duration::from_millis(POLL_MILLIS);
const POLLS_MAX: u64 = GIT_DEADLINE.as_secs() * 1000 / POLL_MILLIS + 1;
const READERS_JOIN_WAIT: Duration = Duration::from_secs(2);
const READERS_JOIN_POLLS_MAX: u64 = READERS_JOIN_WAIT.as_secs() * 1000 / POLL_MILLIS + 1;
const WORKTREE_FIELDS_MAX: usize = 8;
const ARGS_MAX: usize = 12;
const COMMIT_HEX_LENS: [usize; 2] = [40, 64];
const ORIGIN_PREFIX: &str = "origin/";
const HEADS_PREFIX: &str = "refs/heads/";
const BRANCH_FIELD: &[u8] = b"branch refs/heads/";
const WORKTREE_FIELD: &[u8] = b"worktree ";
// Set when rider starts from a git hook, these would point git at another repository.
pub const ENV_REMOVED: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];
const WORKTREE_LIST: [&str; 4] = ["worktree", "list", "--porcelain", "-z"];

#[derive(Debug)]
pub enum GitError {
    Spawn(String),
    /// `git` exited with a failure, or was ended by a signal.
    Exit {
        args: String,
        stderr: String,
    },
    /// `git` printed more than `OUTPUT_BYTES_MAX` bytes on one stream.
    OutputTooLarge {
        args: String,
    },
    /// `git` did not exit and close its output within its deadline.
    TimedOut {
        args: String,
        seconds: u64,
    },
    /// Reading from `git` failed.
    Io(String),
    /// `git` succeeded but printed something rider cannot use.
    Parse {
        args: String,
        stdout: String,
    },
}

impl fmt::Display for GitError {
    /// One line naming the git command that failed and why, with git's own text cut short.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Spawn(message) => {
                write!(f, "git could not be started: {}", board::one_line(message))
            }
            GitError::Exit { args, stderr } if stderr.trim().is_empty() => {
                write!(f, "git {args} failed")
            }
            GitError::Exit { args, stderr } => {
                write!(f, "git {args} failed: {}", board::one_line(stderr))
            }
            GitError::OutputTooLarge { args } => {
                write!(f, "git {args} printed more than {OUTPUT_BYTES_MAX} bytes")
            }
            GitError::TimedOut { args, seconds } => {
                write!(f, "git {args} did not finish within {seconds} seconds")
            }
            GitError::Io(message) => {
                write!(f, "reading from git failed: {}", board::one_line(message))
            }
            GitError::Parse { args, stdout } => {
                write!(
                    f,
                    "git {args} printed output rider cannot use: {}",
                    board::one_line(stdout)
                )
            }
        }
    }
}

type Reader = JoinHandle<Result<Vec<u8>, GitError>>;

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Whether `name` can go to `git` as a ref: not empty, not an option, no whitespace or `..`.
pub fn is_safe_ref(name: &str) -> bool {
    let safe = !name.is_empty()
        && name.len() <= REF_BYTES_MAX
        && !name.starts_with('-')
        && !name.contains("..")
        && !name.contains(char::is_whitespace);
    assert!(!safe || name.len() <= REF_BYTES_MAX);
    assert!(!safe || !name.starts_with('-'));
    safe
}

/// Whether `text` is a full commit ID: 40 or 64 lowercase hex characters.
pub fn is_commit_id(text: &str) -> bool {
    assert!(
        text.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    COMMIT_HEX_LENS.contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn shown(args: &[&OsStr]) -> String {
    assert!(!args.is_empty() && args.len() <= ARGS_MAX);
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let line = words.join(" ");
    assert!(
        line.len() >= args.len() - 1,
        "one separator between each word"
    );
    line
}

fn read_bounded(stream: impl Read, args: &str) -> Result<Vec<u8>, GitError> {
    assert!(!args.is_empty());
    let limit = u64::try_from(OUTPUT_BYTES_MAX + 1).unwrap_or(u64::MAX);
    let mut bytes: Vec<u8> = Vec::new();
    let read = stream.take(limit).read_to_end(&mut bytes);
    read.map_err(|error| GitError::Io(format!("reading git {args}: {error}")))?;
    if bytes.len() > OUTPUT_BYTES_MAX {
        let args = args.to_string();
        return Err(GitError::OutputTooLarge { args });
    }
    assert!(bytes.len() <= OUTPUT_BYTES_MAX);
    Ok(bytes)
}

// A reader that stops at the bound drops its pipe, so git dies writing instead of blocking.
fn start_reader(stream: impl Read + Send + 'static, args: &str) -> Result<Reader, GitError> {
    assert!(!args.is_empty());
    let owned = args.to_string();
    let builder = thread::Builder::new().name("git-output".to_string());
    let reader = builder.spawn(move || read_bounded(stream, &owned));
    reader.map_err(|error| GitError::Spawn(format!("starting a reader thread: {error}")))
}

fn finish_reader(reader: Reader) -> Result<Vec<u8>, GitError> {
    assert!(
        reader.is_finished(),
        "a reader is joined once its pipe closed"
    );
    let joined = reader.join();
    joined.map_err(|_| GitError::Io("a reader thread panicked".to_string()))?
}

/// Starts a reader on each of `child`'s output streams: stdout first, then stderr.
fn start_readers(child: &mut dyn ChildWrapper, args: &str) -> Result<[Reader; 2], GitError> {
    assert!(!args.is_empty());
    let missing = |name: &str| GitError::Spawn(format!("git {args}: no {name}"));
    let stdout = child.stdout().take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr().take().ok_or_else(|| missing("stderr"))?;
    let readers = [start_reader(stdout, args)?, start_reader(stderr, args)?];
    assert!(child.stdout().is_none() && child.stderr().is_none());
    Ok(readers)
}

// `try_wait` reaps the leader, so it is called only once both streams have closed. On every
// error the leader is therefore unreaped, and its id still names its process group.
fn poll(
    child: &mut dyn ChildWrapper,
    readers: &mut [Option<Reader>; 2],
    bytes: &mut [Vec<u8>; 2],
    deadline: Instant,
) -> Result<Option<ExitStatus>, GitError> {
    for _ in 0..POLLS_MAX {
        for (slot, out) in readers.iter_mut().zip(bytes.iter_mut()) {
            if let Some(reader) = slot.take_if(|reader| reader.is_finished()) {
                *out = finish_reader(reader)?;
            }
        }
        if readers.iter().all(Option::is_none) {
            let polled = child.try_wait();
            let status =
                polled.map_err(|error| GitError::Io(format!("waiting for git: {error}")))?;
            if status.is_some() {
                assert!(bytes.iter().all(|out| out.len() <= OUTPUT_BYTES_MAX));
                return Ok(status);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(None)
}

/// Kills `child`'s whole group, which may outlive its leader, then joins the readers that
/// close within `READERS_JOIN_WAIT`.
fn end(
    child: &mut dyn ChildWrapper,
    readers: [Option<Reader>; 2],
    args: &str,
) -> Result<(), GitError> {
    assert!(!args.is_empty());
    let killed = board::kill_group(child);
    let deadline = Instant::now() + READERS_JOIN_WAIT;
    let mut open = readers;
    for _ in 0..READERS_JOIN_POLLS_MAX {
        for slot in &mut open {
            if let Some(reader) = slot.take_if(|reader| reader.is_finished()) {
                // The call has already failed, so what the killed group printed is not wanted.
                let _ = reader.join();
            }
        }
        if open.iter().all(Option::is_none) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    // A reader still open here is held by a process that left the group; it is detached.
    killed.map_err(|error| GitError::Io(format!("ending git {args}: {error}")))
}

/// Reads both streams of `child` until it exits and they close, or until `limit` passes.
/// Unless it ends that way, the whole process group is killed.
fn collect(child: &mut dyn ChildWrapper, args: &str, limit: Duration) -> Result<Output, GitError> {
    assert!(!args.is_empty());
    assert!(limit <= GIT_DEADLINE, "the poll bound covers the deadline");
    let deadline = Instant::now() + limit;
    let mut readers = [None, None];
    let mut bytes = [Vec::new(), Vec::new()];
    let polled = match start_readers(child, args) {
        Ok(started) => {
            readers = started.map(Some);
            poll(child, &mut readers, &mut bytes, deadline)
        }
        Err(error) => Err(error),
    };
    let status = match polled {
        Ok(Some(status)) => status,
        unfinished => {
            end(child, readers, args)?;
            let seconds = limit.as_secs();
            let timed_out = GitError::TimedOut {
                args: args.to_string(),
                seconds,
            };
            return Err(unfinished.err().unwrap_or(timed_out));
        }
    };
    let [stdout, stderr] = bytes;
    let output = Output {
        code: status.code(),
        stdout,
        stderr,
    };
    assert!(output.stdout.len() <= OUTPUT_BYTES_MAX);
    assert!(output.stderr.len() <= OUTPUT_BYTES_MAX);
    Ok(output)
}

fn command(dir: &Path, args: &[&OsStr]) -> Process {
    assert!(!args.is_empty(), "every git run names a command");
    assert!(args.len() <= ARGS_MAX);
    let mut command = Process::new("git");
    command.arg("-C").arg(dir).args(args);
    for name in ENV_REMOVED {
        command.env_remove(name);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Runs `process` in its own process group, feeding it `input` if given, and kills the group
/// unless it ends within `limit`.
pub enum Group {
    Idle,
    Running(Pid),
    Stopped,
}

pub fn spawn_tracked(
    process: Process,
    line: &str,
    limit: Duration,
    slot: &Mutex<Group>,
) -> Result<Output, GitError> {
    assert!(!line.is_empty());
    let mut child = {
        let mut group = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*group, Group::Stopped) {
            return Err(GitError::Spawn(format!("{line}: rider is stopping")));
        }
        let spawned = CommandWrap::from(process)
            .wrap(ProcessGroup::leader())
            .spawn();
        let child = spawned.map_err(|error| GitError::Spawn(format!("{line}: {error}")))?;
        let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
        let Some(pid) = pid else {
            return Err(GitError::Spawn(format!("{line}: no process id")));
        };
        *group = Group::Running(pid);
        child
    };
    let output = collect(child.as_mut(), line, limit);
    let mut group = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if matches!(*group, Group::Running(_)) {
        *group = Group::Idle;
    }
    assert!(!matches!(*group, Group::Running(_)));
    output
}

pub fn stop_tracked(slot: &Mutex<Group>) {
    let mut group = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Group::Running(pid) = *group {
        assert!(pid.as_raw_pid() > 1, "never signal the caller's own group");
        let _ = kill_process_group(pid, Signal::KILL);
    }
    *group = Group::Stopped;
}

pub fn spawn_and_collect(
    process: Process,
    line: &str,
    limit: Duration,
    input: Option<Vec<u8>>,
) -> Result<Output, GitError> {
    assert!(!line.is_empty());
    let spawned = CommandWrap::from(process)
        .wrap(ProcessGroup::leader())
        .spawn();
    let mut child = spawned.map_err(|error| GitError::Spawn(format!("git {line}: {error}")))?;
    let writer = match (input, child.stdin().take()) {
        (Some(bytes), Some(mut stdin)) => {
            // Written from its own thread so git can't block on a full stdout meanwhile.
            let builder = thread::Builder::new().name("git-stdin".to_string());
            match builder.spawn(move || stdin.write_all(&bytes)) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    let _ = board::kill_group(child.as_mut());
                    return Err(GitError::Spawn(format!("git {line}: {error}")));
                }
            }
        }
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            let _ = board::kill_group(child.as_mut());
            return Err(GitError::Spawn(format!("git {line}: stdin is not set up")));
        }
    };
    assert!(child.stdin().is_none());
    let output = collect(child.as_mut(), line, limit);
    // Git has exited or its group was killed, so the writer's pipe has no reader left.
    let written = writer.map(JoinHandle::join);
    match (output, written) {
        // A git that exits without reading all its input reports its own failure.
        (Ok(done), Some(Ok(Err(_)) | Err(_))) if done.code == Some(0) => {
            Err(GitError::Io(format!("writing to git {line}")))
        }
        (output, _) => output,
    }
}

fn run(dir: &Path, args: &[&OsStr]) -> Result<Output, GitError> {
    run_fed(dir, args, None)
}

fn run_fed(dir: &Path, args: &[&OsStr], input: Option<Vec<u8>>) -> Result<Output, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let line = shown(args);
    let mut process = command(dir, args);
    if input.is_some() {
        process.stdin(Stdio::piped());
    }
    spawn_and_collect(process, &line, GIT_DEADLINE, input)
}

fn exit_error(args: &[&OsStr], output: &Output) -> GitError {
    assert!(output.code != Some(0), "only a failed run is an exit error");
    assert!(output.stderr.len() <= OUTPUT_BYTES_MAX);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    GitError::Exit {
        args: shown(args),
        stderr,
    }
}

fn parse_error(args: &[&OsStr], stdout: &[u8]) -> GitError {
    assert!(stdout.len() <= OUTPUT_BYTES_MAX);
    let stdout = String::from_utf8_lossy(stdout).into_owned();
    assert!(!args.is_empty());
    GitError::Parse {
        args: shown(args),
        stdout,
    }
}

fn one_line(args: &[&OsStr], stdout: &[u8]) -> Result<String, GitError> {
    assert!(stdout.len() <= OUTPUT_BYTES_MAX);
    let text = std::str::from_utf8(stdout).map_err(|_| parse_error(args, stdout))?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.is_empty() || line.contains('\n') {
        return Err(parse_error(args, stdout));
    }
    assert!(!line.is_empty() && !line.contains('\n'));
    assert!(line.len() < stdout.len() || !text.ends_with('\n'));
    Ok(line.to_string())
}

fn run_ok(dir: &Path, args: &[&OsStr]) -> Result<Vec<u8>, GitError> {
    let output = run(dir, args)?;
    if output.code != Some(0) {
        return Err(exit_error(args, &output));
    }
    assert!(output.code == Some(0));
    assert!(output.stdout.len() <= OUTPUT_BYTES_MAX);
    Ok(output.stdout)
}

// For the queries here, exit 1 means the thing asked about is absent, not a failure.
fn query(dir: &Path, args: &[&OsStr]) -> Result<Option<String>, GitError> {
    assert!(
        args.len() >= 2,
        "a query names a command and what it asks about"
    );
    let output = run(dir, args)?;
    let answer = match output.code {
        Some(0) => Some(one_line(args, &output.stdout)?),
        Some(1) => None,
        _ => return Err(exit_error(args, &output)),
    };
    assert!(answer.as_ref().is_none_or(|line| !line.contains('\n')));
    Ok(answer)
}

fn commit_id(args: &[&OsStr], stdout: &[u8]) -> Result<String, GitError> {
    let line = one_line(args, stdout)?;
    if !is_commit_id(&line) {
        return Err(parse_error(args, stdout));
    }
    assert!(COMMIT_HEX_LENS.contains(&line.len()));
    assert!(
        is_safe_ref(&line),
        "a commit ID is safe to pass back to git"
    );
    Ok(line)
}

fn absolute_path(args: &[&OsStr], stdout: &[u8]) -> Result<PathBuf, GitError> {
    let line = one_line(args, stdout)?;
    let path = PathBuf::from(line);
    if !path.is_absolute() {
        return Err(parse_error(args, stdout));
    }
    assert!(path.is_absolute());
    assert!(path.components().count() >= 1);
    Ok(path)
}

/// The commit HEAD points at in `dir`.
pub fn head(dir: &Path) -> Result<String, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let args = ["rev-parse", "--verify", "HEAD^{commit}"].map(OsStr::new);
    let stdout = run_ok(dir, &args)?;
    let commit = commit_id(&args, &stdout)?;
    assert!(is_commit_id(&commit));
    Ok(commit)
}

/// The commit `rev` names in the repository at `repo`.
pub fn commit_of(repo: &Path, rev: &str) -> Result<String, GitError> {
    assert!(is_safe_ref(rev), "refs are checked before they reach git");
    let peeled = format!("{rev}^{{commit}}");
    let args = ["rev-parse", "--verify", "--end-of-options", &peeled].map(OsStr::new);
    let stdout = run_ok(repo, &args)?;
    let commit = commit_id(&args, &stdout)?;
    assert!(is_commit_id(&commit));
    Ok(commit)
}

/// Whether `dir` has no modified, staged, or untracked files.
pub fn is_clean(dir: &Path) -> Result<bool, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let args = ["status", "--porcelain", "--untracked-files=normal"].map(OsStr::new);
    let stdout = run_ok(dir, &args)?;
    assert!(stdout.len() <= OUTPUT_BYTES_MAX);
    Ok(stdout.is_empty())
}

/// The commits reachable from `head` and from no other local branch than `branch`, and not
/// from `reference`: the commits only `branch` holds.
pub fn own_commit_count(
    dir: &Path,
    branch: &str,
    head: &str,
    reference: &str,
) -> Result<u64, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    assert!(is_safe_ref(branch) && is_commit_id(head) && is_commit_id(reference));
    // `--exclude` takes the short name, and applies to the `--branches` that follows it.
    let exclude = format!("--exclude={branch}");
    let words = [
        "rev-list",
        "--count",
        head,
        "--not",
        reference,
        &exclude,
        "--branches",
    ];
    let args = words.map(OsStr::new);
    let stdout = run_ok(dir, &args)?;
    let line = one_line(&args, &stdout)?;
    let digits = line.bytes().all(|b| b.is_ascii_digit());
    let count = line.parse::<u64>().ok().filter(|_| digits);
    count.ok_or_else(|| parse_error(&args, &stdout))
}

fn reflog_commits(dir: &Path, log_args: &[&str]) -> Result<Vec<String>, GitError> {
    let args: Vec<&OsStr> = log_args.iter().map(OsStr::new).collect();
    let stdout = run_ok(dir, &args)?;
    let text = std::str::from_utf8(&stdout).map_err(|_| parse_error(&args, &stdout))?;
    let mut commits: Vec<String> = text.lines().map(str::to_string).collect();
    if !commits.iter().all(|commit| is_commit_id(commit)) {
        return Err(parse_error(&args, &stdout));
    }
    commits.sort_unstable();
    commits.dedup();
    assert!(commits.len() <= stdout.len());
    Ok(commits)
}

// A reflog record is `<old id> <new id> <identity>\t<message>`; a ref's first record has an
// all-zero old id. Neither id is guaranteed to be reachable from anything else.
fn head_reflog_ids(dir: &Path) -> Result<Vec<String>, GitError> {
    let words = [
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "logs/HEAD",
    ];
    let args = words.map(OsStr::new);
    let stdout = run_ok(dir, &args)?;
    let path = absolute_path(&args, &stdout)?;
    let shown_path = path.display().to_string();
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(GitError::Io(format!("opening {shown_path}: {error}"))),
    };
    let bytes = read_bounded(&mut file, &shown_path)?;
    let malformed = |line: &[u8]| GitError::Parse {
        args: shown_path.clone(),
        stdout: String::from_utf8_lossy(line).into_owned(),
    };
    let mut ids: Vec<String> = Vec::new();
    for line in bytes.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, |&b| b == b' ');
        for _ in 0..2 {
            let field = fields.next().ok_or_else(|| malformed(line))?;
            let id = std::str::from_utf8(field).map_err(|_| malformed(line))?;
            if !is_commit_id(id) {
                return Err(malformed(line));
            }
            if id.bytes().any(|b| b != b'0') {
                ids.push(id.to_string());
            }
        }
    }
    assert!(ids.len() <= bytes.len());
    assert!(ids.iter().all(|id| is_commit_id(id)));
    Ok(ids)
}

/// The commits `dir`'s own HEAD, or either id of any record of its HEAD reflog, reaches that
/// no local branch nor any branch's reflog does: the commits lost when the worktree, and with
/// it that reflog, is removed.
pub fn head_reflog_only_commit_count(dir: &Path) -> Result<u64, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let mut reached = head_reflog_ids(dir)?;
    reached.push(head(dir)?);
    reached.sort_unstable();
    reached.dedup();
    assert!(!reached.is_empty());
    let kept = reflog_commits(dir, &["log", "-g", "--format=%H", "--branches", "--"])?;
    let mut input = String::new();
    for commit in &reached {
        input.push_str(commit);
        input.push('\n');
    }
    for commit in &kept {
        input.push('^');
        input.push_str(commit);
        input.push('\n');
    }
    assert!(input.lines().count() == reached.len() + kept.len());
    // A record's id may name a commit already pruned, which removal can no longer lose.
    let words = [
        "rev-list",
        "--count",
        "--ignore-missing",
        "--stdin",
        "--not",
        "--branches",
    ];
    let args = words.map(OsStr::new);
    let output = run_fed(dir, &args, Some(input.into_bytes()))?;
    if output.code != Some(0) {
        return Err(exit_error(&args, &output));
    }
    let line = one_line(&args, &output.stdout)?;
    let digits = line.bytes().all(|b| b.is_ascii_digit());
    let count = line.parse::<u64>().ok().filter(|_| digits);
    count.ok_or_else(|| parse_error(&args, &output.stdout))
}

/// Whether `commit` names a commit in `dir`'s repository.
pub fn commit_exists(dir: &Path, commit: &str) -> Result<bool, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    assert!(is_commit_id(commit));
    let object = format!("{commit}^{{commit}}");
    let args = ["cat-file", "-e", &object].map(OsStr::new);
    let output = run(dir, &args)?;
    // git exits 128, not 1, for an object that is missing or is not a commit.
    Ok(output.code == Some(0))
}

/// Whether `ancestor` is `descendant` or one of its ancestors, in `dir`'s repository.
pub fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    assert!(is_commit_id(ancestor) && is_commit_id(descendant));
    let args = ["merge-base", "--is-ancestor", ancestor, descendant].map(OsStr::new);
    let output = run(dir, &args)?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(exit_error(&args, &output)),
    }
}

/// Whether the local branch `branch` exists in the repository at `repo`.
pub fn branch_exists(repo: &Path, branch: &str) -> Result<bool, GitError> {
    assert!(
        is_safe_ref(branch),
        "refs are checked before they reach git"
    );
    let full = format!("refs/heads/{branch}");
    assert!(full.len() > branch.len());
    let args = ["show-ref", "--verify", "--quiet", &full].map(OsStr::new);
    let output = run(repo, &args)?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(exit_error(&args, &output)),
    }
}

/// The branch checked out in `dir`, or `None` when HEAD is detached.
pub fn current_branch(dir: &Path) -> Result<Option<String>, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    // `--short` prints `heads/<name>` when a tag shares the branch's name, so strip the full ref.
    let args = ["symbolic-ref", "--quiet", "HEAD"].map(OsStr::new);
    let Some(full) = query(dir, &args)? else {
        return Ok(None);
    };
    let branch = full.strip_prefix(HEADS_PREFIX).filter(|b| !b.is_empty());
    let branch = branch.ok_or_else(|| parse_error(&args, full.as_bytes()))?;
    assert!(!branch.is_empty() && full.len() > branch.len());
    Ok(Some(branch.to_string()))
}

/// The branch `origin/HEAD` names, or `None` when the repository has no `origin/HEAD`.
pub fn default_branch(repo: &Path) -> Result<Option<String>, GitError> {
    assert!(repo.is_absolute(), "git runs in absolute directories");
    let args = [
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ];
    let args = args.map(OsStr::new);
    let Some(remote) = query(repo, &args)? else {
        return Ok(None);
    };
    let branch = remote.strip_prefix(ORIGIN_PREFIX);
    let Some(branch) = branch.filter(|branch| is_safe_ref(branch)) else {
        return Err(parse_error(&args, remote.as_bytes()));
    };
    assert!(is_safe_ref(branch) && branch.len() < remote.len());
    Ok(Some(branch.to_string()))
}

/// Whether `dir` is the top of a working tree: inside one, with no prefix below its top.
pub fn is_toplevel(dir: &Path) -> Result<bool, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let args = ["rev-parse", "--is-inside-work-tree", "--show-prefix"].map(OsStr::new);
    let output = run(dir, &args)?;
    // Any failure means `dir` is not in a working tree, which is the answer, not an error.
    let toplevel = output.code == Some(0) && output.stdout == b"true\n\n";
    assert!(output.stdout.len() <= OUTPUT_BYTES_MAX);
    Ok(toplevel)
}

/// The absolute directory of the repository `dir` belongs to, shared by its worktrees.
pub fn common_dir(dir: &Path) -> Result<PathBuf, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"].map(OsStr::new);
    let stdout = run_ok(dir, &args)?;
    let path = absolute_path(&args, &stdout)?;
    assert!(path.is_absolute());
    Ok(path)
}

/// The absolute git directory of the worktree at `dir`; the main checkout's is `common_dir`.
pub fn git_dir(dir: &Path) -> Result<PathBuf, GitError> {
    assert!(dir.is_absolute(), "git runs in absolute directories");
    let args = ["rev-parse", "--absolute-git-dir"].map(OsStr::new);
    let stdout = run_ok(dir, &args)?;
    let path = absolute_path(&args, &stdout)?;
    assert!(path.is_absolute());
    Ok(path)
}

fn parse_worktree_branches(stdout: &[u8]) -> Result<Vec<(PathBuf, String)>, GitError> {
    assert!(
        stdout.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    let fields_max = WORKTREES_MAX * WORKTREE_FIELDS_MAX;
    let malformed = || GitError::Parse {
        args: WORKTREE_LIST.join(" "),
        stdout: String::from_utf8_lossy(stdout).into_owned(),
    };
    let mut branches: Vec<(PathBuf, String)> = Vec::new();
    let mut dir: Option<PathBuf> = None;
    let mut fields = stdout.split(|&byte| byte == 0);
    for field in fields.by_ref().take(fields_max) {
        if let Some(path) = field.strip_prefix(WORKTREE_FIELD) {
            dir = Some(PathBuf::from(OsStr::from_bytes(path)));
            continue;
        }
        let Some(name) = field.strip_prefix(BRANCH_FIELD) else {
            continue;
        };
        // A branch rider could never name cannot be the one it asks about, so it is skipped.
        let Some(name) = std::str::from_utf8(name)
            .ok()
            .filter(|name| is_safe_ref(name))
        else {
            continue;
        };
        let Some(dir) = dir.clone() else {
            return Err(malformed());
        };
        if branches.len() == WORKTREES_MAX {
            return Err(malformed());
        }
        branches.push((dir, name.to_string()));
    }
    if fields.next().is_some() {
        return Err(malformed());
    }
    assert!(branches.len() <= WORKTREES_MAX);
    assert!(branches.iter().all(|(_, branch)| is_safe_ref(branch)));
    Ok(branches)
}

/// Each worktree of `repo` on a local branch, the main checkout included, with that branch.
pub fn worktree_branches(repo: &Path) -> Result<Vec<(PathBuf, String)>, GitError> {
    assert!(repo.is_absolute(), "git runs in absolute directories");
    let args = WORKTREE_LIST.map(OsStr::new);
    let stdout = run_ok(repo, &args)?;
    let branches = parse_worktree_branches(&stdout)?;
    assert!(branches.len() <= WORKTREES_MAX);
    Ok(branches)
}

/// Adds a worktree at `dir` on the new branch `branch`, cut from `commit`.
pub fn add_on_new_branch(
    repo: &Path,
    dir: &Path,
    branch: &str,
    commit: &str,
) -> Result<(), GitError> {
    assert!(is_safe_ref(branch) && is_commit_id(commit));
    assert!(
        dir.is_absolute(),
        "worktrees live under rider's absolute home"
    );
    let args = [
        OsStr::new("worktree"),
        OsStr::new("add"),
        OsStr::new("-b"),
        OsStr::new(branch),
        OsStr::new("--"),
        dir.as_os_str(),
        OsStr::new(commit),
    ];
    run_ok(repo, &args).map(|_| ())
}

/// Adds a worktree at `dir` on the existing branch `branch`.
pub fn add_on_branch(repo: &Path, dir: &Path, branch: &str) -> Result<(), GitError> {
    assert!(is_safe_ref(branch));
    assert!(
        dir.is_absolute(),
        "worktrees live under rider's absolute home"
    );
    let words = ["worktree", "add", "--"].map(OsStr::new);
    let args = [
        words[0],
        words[1],
        words[2],
        dir.as_os_str(),
        OsStr::new(branch),
    ];
    run_ok(repo, &args).map(|_| ())
}

/// Adds a worktree at `dir`, detached at `commit`.
pub fn add_detached(repo: &Path, dir: &Path, commit: &str) -> Result<(), GitError> {
    assert!(is_commit_id(commit));
    assert!(
        dir.is_absolute(),
        "worktrees live under rider's absolute home"
    );
    let words = ["worktree", "add", "--detach", "--"].map(OsStr::new);
    let args = [
        words[0],
        words[1],
        words[2],
        words[3],
        dir.as_os_str(),
        OsStr::new(commit),
    ];
    run_ok(repo, &args).map(|_| ())
}

/// Removes the linked worktree at `dir` without force, keeping its branch.
pub fn remove_worktree(repo: &Path, dir: &Path) -> Result<(), GitError> {
    assert!(
        dir.is_absolute(),
        "worktrees live under rider's absolute home"
    );
    assert!(repo.is_absolute(), "git runs in absolute directories");
    let words = ["worktree", "remove", "--"].map(OsStr::new);
    let args = [words[0], words[1], words[2], dir.as_os_str()];
    run_ok(repo, &args).map(|_| ())
}

#[cfg(test)]
pub mod testing {
    //! Real repositories in temporary directories, for tests in this crate.

    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{Output, command, shown, spawn_and_collect};

    // Config from the environment outranks repository config and may sign commits or run hooks.
    fn test_git(dir: &Path, args: &[&str]) -> Output {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let mut process = command(dir, &args);
        process.env_remove("GIT_CONFIG_COUNT");
        process.env_remove("GIT_CONFIG_PARAMETERS");
        spawn_and_collect(process, &shown(&args), super::GIT_DEADLINE, None).unwrap()
    }

    /// Runs `git -C <dir> <args>`, asserts it succeeded, and returns its trimmed stdout.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let output = test_git(dir, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.code, Some(0), "git {args:?} failed: {stderr}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A temporary directory with a one-commit repository `repo` and an empty `home`.
    pub struct Fixture {
        pub root: PathBuf,
        pub repo: PathBuf,
        pub home: PathBuf,
        _dir: TempDir,
    }

    impl Fixture {
        pub fn new(name: &str) -> Fixture {
            let dir = tempfile::Builder::new()
                .prefix(&format!("rider-{name}-"))
                .tempdir()
                .unwrap();
            let root = dir.path().to_path_buf();
            let repo = root.join("repo");
            let home = root.join("home");
            fs::create_dir_all(&repo).unwrap();
            fs::create_dir_all(&home).unwrap();
            init_repo(&repo);
            Fixture {
                root,
                repo,
                home,
                _dir: dir,
            }
        }

        /// Commits a change to `file` on the current branch of `dir` and returns the commit.
        pub fn commit(&self, dir: &Path, file: &str, text: &str) -> String {
            fs::write(dir.join(file), text).unwrap();
            git(dir, &["add", "--", file]);
            git(dir, &["commit", "-q", "-m", text]);
            git(dir, &["rev-parse", "HEAD"])
        }

        /// Makes `branch` one commit ahead of `main`, leaving `main` checked out; returns its head.
        pub fn branch_ahead(&self, branch: &str) -> String {
            git(&self.repo, &["checkout", "-q", "-b", branch]);
            let head = self.commit(&self.repo, "ahead.txt", branch);
            git(&self.repo, &["checkout", "-q", "main"]);
            head
        }
    }

    /// Makes `dir` a repository on `main` with one commit, set up to commit on any machine.
    pub fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "Rider Test"]);
        git(dir, &["config", "user.email", "rider@example.invalid"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        fs::write(dir.join("README"), "hello\n").unwrap();
        git(dir, &["add", "README"]);
        git(dir, &["commit", "-q", "-m", "Start."]);
    }
}
