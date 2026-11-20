//! Thin wrappers over the `git` command-line program, one process per call.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};

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
// Set when rider starts from a git hook, these would point git at another repository.
const ENV_REMOVED: [&str; 8] = [
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

struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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
fn spawn_and_collect(
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

fn parse_worktree_branches(stdout: &[u8]) -> Result<Vec<String>, GitError> {
    assert!(
        stdout.len() <= OUTPUT_BYTES_MAX,
        "output is bounded before it is parsed"
    );
    let fields_max = WORKTREES_MAX * WORKTREE_FIELDS_MAX;
    let malformed = || GitError::Parse {
        args: WORKTREE_LIST.join(" "),
        stdout: String::from_utf8_lossy(stdout).into_owned(),
    };
    let mut branches: Vec<String> = Vec::new();
    let mut fields = stdout.split(|&byte| byte == 0);
    for field in fields.by_ref().take(fields_max) {
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
        if branches.len() == WORKTREES_MAX {
            return Err(malformed());
        }
        branches.push(name.to_string());
    }
    if fields.next().is_some() {
        return Err(malformed());
    }
    assert!(branches.len() <= WORKTREES_MAX);
    assert!(branches.iter().all(|branch| is_safe_ref(branch)));
    Ok(branches)
}

/// The local branches checked out in any worktree of `repo`, the main checkout included.
pub fn worktree_branches(repo: &Path) -> Result<Vec<String>, GitError> {
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

    /// Whether `git -C <dir> <args>` succeeds.
    pub fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
        test_git(dir, args).code == Some(0)
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

        /// Points `origin/HEAD` at a remote-tracking copy of `branch`, as a clone would.
        pub fn set_origin_head(&self, branch: &str) {
            let tracking = format!("refs/remotes/origin/{branch}");
            git(&self.repo, &["update-ref", &tracking, branch]);
            let head = "refs/remotes/origin/HEAD";
            git(&self.repo, &["symbolic-ref", head, &tracking]);
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

#[cfg(test)]
mod tests {
    use super::testing::{Fixture, git, init_repo};
    use super::*;
    use std::fs;

    #[test]
    fn every_git_child_ignores_an_inherited_repository() {
        let args = ["status"].map(OsStr::new);
        let command = command(Path::new("/"), &args);
        let envs: Vec<_> = command.get_envs().collect();
        let removed = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ];
        for name in removed {
            assert!(
                envs.contains(&(OsStr::new(name), None)),
                "{name} is removed"
            );
        }
        assert!(envs.contains(&(OsStr::new("GIT_TERMINAL_PROMPT"), Some(OsStr::new("0")))));
    }

    #[test]
    fn a_worktree_on_an_unsafe_branch_name_is_skipped() {
        let list = b"worktree /r\0HEAD 0123\0branch refs/heads/main\0\0\
worktree /w\0HEAD 0123\0branch refs/heads/-odd\0\0\
worktree /v\0HEAD 0123\0branch refs/heads/rider/2-y\0\0";
        let branches = parse_worktree_branches(list).unwrap();
        assert_eq!(branches, vec!["main".to_string(), "rider/2-y".to_string()]);
    }

    #[test]
    fn worktree_branches_lists_every_checked_out_branch() {
        let fixture = Fixture::new("git-list");
        fixture.branch_ahead("rider/1-x");
        let linked = fixture.root.join("linked");
        let path = linked.to_str().unwrap();
        git(
            &fixture.repo,
            &["worktree", "add", "-q", "--", path, "rider/1-x"],
        );
        let detached = fixture.root.join("detached");
        let path = detached.to_str().unwrap();
        git(
            &fixture.repo,
            &["worktree", "add", "-q", "--detach", "--", path, "main"],
        );
        let branches = worktree_branches(&fixture.repo).unwrap();
        assert_eq!(branches, vec!["main".to_string(), "rider/1-x".to_string()]);
    }

    #[test]
    fn head_is_the_commit_head_points_at() {
        let fixture = Fixture::new("git-head");
        let expected = git(&fixture.repo, &["rev-parse", "HEAD"]);
        assert_eq!(head(&fixture.repo).unwrap(), expected);
        let nowhere = fixture.root.join("missing");
        assert!(matches!(head(&nowhere), Err(GitError::Exit { .. })));
    }

    #[test]
    fn a_modified_or_untracked_file_is_not_clean() {
        let fixture = Fixture::new("git-clean");
        assert!(is_clean(&fixture.repo).unwrap());
        fs::write(fixture.repo.join("new.txt"), "x").unwrap();
        assert!(!is_clean(&fixture.repo).unwrap());
        fs::remove_file(fixture.repo.join("new.txt")).unwrap();
        fs::write(fixture.repo.join("README"), "changed\n").unwrap();
        assert!(!is_clean(&fixture.repo).unwrap());
    }

    #[test]
    fn own_commits_are_those_no_other_branch_holds() {
        let fixture = Fixture::new("git-own");
        let start = head(&fixture.repo).unwrap();
        let repo = &fixture.repo;
        git(repo, &["checkout", "-q", "-b", "rider/7-x"]);
        assert_eq!(
            own_commit_count(repo, "rider/7-x", &start, &start).unwrap(),
            0
        );
        let one = fixture.commit(repo, "a.txt", "one");
        let two = fixture.commit(repo, "b.txt", "two");
        assert_eq!(
            own_commit_count(repo, "rider/7-x", &two, &start).unwrap(),
            2
        );
        assert_eq!(own_commit_count(repo, "rider/7-x", &two, &one).unwrap(), 1);
        git(repo, &["branch", "sibling", &one]);
        assert_eq!(
            own_commit_count(repo, "rider/7-x", &two, &start).unwrap(),
            1
        );
        git(repo, &["branch", "-f", "main", &two]);
        assert_eq!(
            own_commit_count(repo, "rider/7-x", &two, &start).unwrap(),
            0
        );
    }

    #[test]
    fn commit_exists_only_for_commits_in_the_repository() {
        let fixture = Fixture::new("git-exists");
        let start = head(&fixture.repo).unwrap();
        assert!(commit_exists(&fixture.repo, &start).unwrap());
        assert!(!commit_exists(&fixture.repo, &"0".repeat(40)).unwrap());
        let tree = git(&fixture.repo, &["rev-parse", "HEAD^{tree}"]);
        assert!(
            !commit_exists(&fixture.repo, &tree).unwrap(),
            "a tree is not a commit"
        );
    }

    #[test]
    fn current_branch_is_not_confused_by_a_tag_of_the_same_name() {
        let fixture = Fixture::new("git-tag-name");
        git(&fixture.repo, &["tag", "main"]);
        assert_eq!(
            current_branch(&fixture.repo).unwrap().as_deref(),
            Some("main")
        );
        git(&fixture.repo, &["checkout", "-q", "--detach"]);
        assert_eq!(current_branch(&fixture.repo).unwrap(), None);
    }

    #[test]
    fn is_ancestor_follows_history_only_backwards() {
        let fixture = Fixture::new("git-ancestor");
        let start = head(&fixture.repo).unwrap();
        assert!(is_ancestor(&fixture.repo, &start, &start).unwrap());
        let end = fixture.commit(&fixture.repo, "a.txt", "one");
        assert!(is_ancestor(&fixture.repo, &start, &end).unwrap());
        assert!(!is_ancestor(&fixture.repo, &end, &start).unwrap());
        let missing = "0".repeat(40);
        let result = is_ancestor(&fixture.repo, &missing, &end);
        assert!(matches!(result, Err(GitError::Exit { .. })), "{result:?}");
    }

    #[test]
    fn branch_exists_only_for_local_branches() {
        let fixture = Fixture::new("git-branch");
        assert!(branch_exists(&fixture.repo, "main").unwrap());
        assert!(!branch_exists(&fixture.repo, "rider/1-x").unwrap());
        fixture.branch_ahead("rider/1-x");
        assert!(branch_exists(&fixture.repo, "rider/1-x").unwrap());
    }

    #[test]
    fn default_branch_prefers_origin_head() {
        let fixture = Fixture::new("git-default");
        let clone = fixture.root.join("clone");
        let source = fixture.repo.to_str().unwrap();
        git(&fixture.root, &["clone", "-q", "--", source, "clone"]);
        git(&clone, &["checkout", "-q", "-b", "elsewhere"]);
        assert_eq!(default_branch(&clone).unwrap().as_deref(), Some("main"));
    }

    #[test]
    fn without_origin_head_there_is_no_default_branch() {
        let fixture = Fixture::new("git-current");
        git(&fixture.repo, &["checkout", "-q", "-b", "trunk"]);
        assert_eq!(default_branch(&fixture.repo).unwrap(), None);
        let source = fixture.root.join("source");
        git(
            &fixture.repo,
            &["remote", "add", "origin", source.to_str().unwrap()],
        );
        assert_eq!(default_branch(&fixture.repo).unwrap(), None);
    }

    #[test]
    fn a_subdirectory_answers_for_its_repository() {
        let fixture = Fixture::new("git-subdir");
        let sub = fixture.repo.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fixture.set_origin_head("main");
        assert_eq!(default_branch(&sub).unwrap().as_deref(), Some("main"));
        assert!(branch_exists(&sub, "main").unwrap());
        let other = fixture.root.join("other");
        fs::create_dir_all(&other).unwrap();
        init_repo(&other);
        assert!(is_clean(&other).unwrap());
    }

    fn is_gone(pid: i32) -> bool {
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        for _ in 0..1000 {
            if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_hook_left_holding_output_is_killed_at_the_deadline() {
        let fixture = Fixture::new("git-hook");
        let hooks = fixture.root.join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let pid_file = fixture.root.join("sleeper.pid");
        let hook = format!(
            "#!/bin/sh\nsleep 60 &\necho $! > '{}'\n",
            pid_file.display()
        );
        let hook_path = hooks.join("post-checkout");
        fs::write(&hook_path, hook).unwrap();
        let executable = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        fs::set_permissions(&hook_path, executable).unwrap();
        git(
            &fixture.repo,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        let linked = fixture.root.join("linked");
        let args = [OsStr::new("worktree"), OsStr::new("add"), OsStr::new("-q")];
        let args = [
            args[0],
            args[1],
            args[2],
            OsStr::new("--"),
            linked.as_os_str(),
        ];
        let limit = Duration::from_secs(5);
        let started = Instant::now();
        let result = spawn_and_collect(command(&fixture.repo, &args), &shown(&args), limit, None);
        let elapsed = started.elapsed();
        let error = result.err();
        assert!(
            matches!(error, Some(GitError::TimedOut { seconds: 5, .. })),
            "{error:?}"
        );
        assert!(elapsed < limit * 3, "returned after {elapsed:?}");
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            is_gone(pid),
            "the hook's background sleep {pid} outlived the call"
        );
    }

    #[test]
    fn output_past_the_bound_fails_without_waiting_for_the_deadline() {
        let fixture = Fixture::new("git-overflow");
        let mut process = Process::new("sh");
        let script = format!("head -c {} /dev/zero; exec sleep 60", OUTPUT_BYTES_MAX + 1);
        process.arg("-c").arg(script).current_dir(&fixture.root);
        process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let result = spawn_and_collect(process, "overflow", Duration::from_secs(60), None);
        let elapsed = started.elapsed();
        let error = result.err();
        assert!(
            matches!(&error, Some(GitError::OutputTooLarge { args }) if args == "overflow"),
            "{error:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "returned after {elapsed:?}"
        );
    }
}
