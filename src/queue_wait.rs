use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use serde_json::Value;

use crate::board::{ListedTask, TaskId};
use crate::git;
use crate::queue::QUEUE_ITEMS_MAX;

pub const PACKET_POLL_SECONDS_MIN: u64 = 10;
pub const PACKET_POLL_SECONDS_MAX: u64 = 60;
pub const GITHUB_POLL_SECONDS_MIN: u64 = 60;
pub const GITHUB_POLL_SECONDS_MAX: u64 = 300;
const CALL_SECONDS_MAX: u64 = 30;
const CALL_POLL_MILLIS: u64 = 50;
const QUEUE_POLLS_MAX: u32 = 4096;
const REPLY_BYTES_MAX: usize = 16 * 1024 * 1024;
const THREADS_MAX: usize = 1024;
const COMMENTS_MAX: usize = 4096;
const SLUG_BYTES_MAX: usize = 64;
const LABEL_SHOWN_CHARS_MAX: usize = 48;
const WARNINGS_MAX: usize = 16;
const REVIEWED_DIR: &str = "queue/reviewed";
const RECORD_BYTES_MAX: u64 = 64;
const LOGIN_BYTES_MAX: usize = 64;
const CALLS_IN_FLIGHT_MAX: usize = 16;

#[derive(Debug)]
pub enum WaitError {
    Spawn {
        program: &'static str,
        message: String,
    },
    Exit {
        program: &'static str,
        code: Option<i32>,
    },
    TimedOut(&'static str),
    Stopped(&'static str),
    TooLarge(&'static str),
    Field {
        program: &'static str,
        field: String,
        problem: &'static str,
    },
    Label {
        task: TaskId,
        key: &'static str,
        value: String,
    },
    Record {
        path: PathBuf,
        problem: String,
    },
    AgentRecord,
    TooManyItems,
    TooManyPolls,
    PollPanicked,
}

impl WaitError {
    fn is_malformed(&self) -> bool {
        match self {
            WaitError::TooLarge(_)
            | WaitError::Field { .. }
            | WaitError::Label { .. }
            | WaitError::Record { .. }
            | WaitError::AgentRecord => true,
            WaitError::Spawn { .. }
            | WaitError::Exit { .. }
            | WaitError::TimedOut(_)
            | WaitError::Stopped(_)
            | WaitError::TooManyItems
            | WaitError::TooManyPolls
            | WaitError::PollPanicked => false,
        }
    }
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitError::Spawn { program, message } => {
                write!(f, "could not run {program}: {message}")
            }
            WaitError::Exit { program, code } => match code {
                Some(code) => write!(f, "{program} exited {code}"),
                None => write!(f, "{program} was stopped by a signal"),
            },
            WaitError::TimedOut(program) => {
                write!(
                    f,
                    "{program} did not finish within {CALL_SECONDS_MAX} seconds"
                )
            }
            WaitError::Stopped(program) => write!(f, "{program} was stopped at the deadline"),
            WaitError::TooLarge(program) => {
                write!(f, "{program} printed more than {REPLY_BYTES_MAX} bytes")
            }
            WaitError::Field {
                program,
                field,
                problem,
            } => write!(f, "{program} reply: {field} {problem}"),
            WaitError::Label { task, key, value } => {
                write!(
                    f,
                    "queue task #{}: label {key}={value:?} is malformed",
                    task.0
                )
            }
            WaitError::Record { path, problem } => {
                write!(f, "the reviewed head in {}: {problem}", path.display())
            }
            WaitError::AgentRecord => {
                write!(f, "an agent's task may not record a reviewed head")
            }
            WaitError::TooManyItems => {
                write!(f, "the queue holds more than {QUEUE_ITEMS_MAX} items")
            }
            WaitError::TooManyPolls => write!(f, "the wait polled {QUEUE_POLLS_MAX} times"),
            WaitError::PollPanicked => write!(f, "a poll thread panicked"),
        }
    }
}

impl std::error::Error for WaitError {}

pub enum Watch {
    Packet {
        pr: String,
        slug: String,
        after: Option<Timestamp>,
    },
    Pull {
        pr: String,
        repo: String,
        number: u64,
        head: Option<String>,
        after: Option<Timestamp>,
        comments: bool,
    },
    Unrecorded {
        pr: String,
    },
}

pub trait Sources: Sync {
    fn packet(&self, slug: &str, deadline: Instant) -> Result<Vec<u8>, WaitError>;
    fn pull(&self, repo: &str, number: u64, deadline: Instant) -> Result<Vec<u8>, WaitError>;
}

pub struct Cli;

impl Sources for Cli {
    // `reviews threads` prints neither comment times nor replies, so the packet's comments
    // are read from `reviews show`; a `reviews wait --follow` would replace this one call.
    fn packet(&self, slug: &str, deadline: Instant) -> Result<Vec<u8>, WaitError> {
        assert!(!slug.starts_with('-'));
        run("reviews", &["show", slug], deadline)
    }

    fn pull(&self, repo: &str, number: u64, deadline: Instant) -> Result<Vec<u8>, WaitError> {
        assert!(!repo.starts_with('-'));
        let number = number.to_string();
        let fields = "headRefOid,comments,reviews";
        let args = ["pr", "view", &number, "--repo", repo, "--json", fields];
        run("gh", &args, deadline)
    }
}

fn run(program: &'static str, args: &[&str], deadline: Instant) -> Result<Vec<u8>, WaitError> {
    assert!(!args.is_empty());
    let spawn_error = |error: std::io::Error| WaitError::Spawn {
        program,
        message: error.to_string(),
    };
    let mut process = Command::new(program);
    process.args(args).stdin(Stdio::null());
    process.stdout(Stdio::piped()).stderr(Stdio::null());
    let wrapped = CommandWrap::from(process)
        .wrap(ProcessGroup::leader())
        .spawn();
    let mut child = wrapped.map_err(spawn_error)?;
    let stdout = child.stdout().take();
    let (send, receive) = sync_channel(1);
    let reader = thread::Builder::new().spawn(move || {
        let mut out = Vec::new();
        let bound = u64::try_from(REPLY_BYTES_MAX)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let read = stdout.map(|stdout| stdout.take(bound).read_to_end(&mut out));
        let _ = send.send(read.unwrap_or(Ok(0)).map(|_| out));
    });
    let call_limit = Instant::now() + Duration::from_secs(CALL_SECONDS_MAX);
    let limit = deadline.min(call_limit);
    let stopped = || {
        if limit < call_limit {
            WaitError::Stopped(program)
        } else {
            WaitError::TimedOut(program)
        }
    };
    let stop = |child: &mut Box<dyn ChildWrapper>| {
        let _ = child.kill();
        let _ = child.wait();
        Err(stopped())
    };
    if reader.is_err() {
        let _ = stop(&mut child);
        return Err(WaitError::PollPanicked);
    }
    // A process the child started may hold standard output after the child exits, so the
    // read, not only the exit, is bounded by the limit.
    let Ok(out) = receive.recv_timeout(limit.saturating_duration_since(Instant::now())) else {
        return stop(&mut child);
    };
    let status = loop {
        if let Some(status) = child.try_wait().map_err(spawn_error)? {
            break status;
        }
        if Instant::now() >= limit {
            return stop(&mut child);
        }
        thread::sleep(Duration::from_millis(CALL_POLL_MILLIS));
    };
    let out = out.map_err(spawn_error)?;
    if out.len() > REPLY_BYTES_MAX {
        return Err(WaitError::TooLarge(program));
    }
    if !status.success() {
        let code = status.code();
        return Err(WaitError::Exit { program, code });
    }
    assert!(out.len() <= REPLY_BYTES_MAX);
    Ok(out)
}

fn label<'a>(task: &'a ListedTask, key: &str) -> Option<&'a str> {
    let found = task.view.labels.iter().find(|(known, _)| known == key);
    found.map(|(_, value)| value.as_str())
}

fn checked<'a, T>(
    task: &'a ListedTask,
    key: &'static str,
    parse: impl Fn(&'a str) -> Option<T>,
) -> Result<Option<T>, WaitError> {
    let Some(value) = label(task, key) else {
        return Ok(None);
    };
    if let Some(parsed) = parse(value) {
        return Ok(Some(parsed));
    }
    let value = value.chars().take(LABEL_SHOWN_CHARS_MAX).collect();
    Err(WaitError::Label {
        task: task.view.id,
        key,
        value,
    })
}

fn parse_pr(value: &str) -> Option<(&str, u64)> {
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let (repo, number) = value.split_once('#')?;
    let (owner, name) = repo.split_once('/')?;
    let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    let number = number.parse().ok().filter(|_| digits)?;
    (part(owner) && part(name)).then_some((repo, number))
}

fn parse_slug(slug: &str) -> Option<&str> {
    let valid = !slug.is_empty()
        && slug.len() <= SLUG_BYTES_MAX
        && !slug.starts_with('-')
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b));
    valid.then_some(slug)
}

fn record_path(home: &Path, item: TaskId) -> PathBuf {
    assert!(home.is_absolute());
    let path = home.join(REVIEWED_DIR).join(item.0.to_string());
    assert!(path.starts_with(home));
    path
}

pub fn recorded_head(home: &Path, item: TaskId) -> Result<Option<String>, WaitError> {
    let path = record_path(home, item);
    let problem = |problem: String| WaitError::Record {
        path: path.clone(),
        problem,
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(problem(error.to_string())),
    };
    let mut text = String::new();
    let read = file.take(RECORD_BYTES_MAX).read_to_string(&mut text);
    read.map_err(|error| problem(error.to_string()))?;
    let head = text
        .strip_suffix('\n')
        .filter(|head| git::is_commit_id(head));
    let head = head.ok_or_else(|| problem("is not one commit id".to_string()))?;
    assert!(head.len() as u64 <= RECORD_BYTES_MAX);
    Ok(Some(head.to_string()))
}

pub fn record_head(home: &Path, item: TaskId, head: &str) -> Result<(), WaitError> {
    assert!(git::is_commit_id(head));
    if env::var_os("BOARD_TASK").is_some() {
        return Err(WaitError::AgentRecord);
    }
    let path = record_path(home, item);
    let problem = |error: std::io::Error| WaitError::Record {
        path: path.clone(),
        problem: error.to_string(),
    };
    let dir = path
        .parent()
        .ok_or_else(|| problem(ErrorKind::NotFound.into()))?;
    fs::create_dir_all(dir).map_err(problem)?;
    let partial = dir.join(format!(".{}.partial", item.0));
    fs::write(&partial, format!("{head}\n")).map_err(problem)?;
    fs::rename(&partial, &path).map_err(problem)?;
    assert_eq!(recorded_head(home, item)?.as_deref(), Some(head));
    Ok(())
}

pub fn watches(home: &Path, tasks: &[ListedTask]) -> Result<Vec<Watch>, WaitError> {
    if tasks.len() > QUEUE_ITEMS_MAX {
        return Err(WaitError::TooManyItems);
    }
    let time = |value: &str| value.parse::<Timestamp>().ok();
    let commit = |value: &str| git::is_commit_id(value).then(|| value.to_string());
    let mut watches = Vec::with_capacity(tasks.len().saturating_mul(2));
    for task in tasks {
        let Some((repo, number)) = checked(task, "pr", parse_pr)? else {
            continue;
        };
        let pr = format!("{repo}#{number}");
        let turn = label(task, "turn");
        let slug = checked(task, "packet", parse_slug)?;
        if let (Some(slug), Some("owner")) = (slug, turn) {
            let after = checked(task, "packet_at", time)?;
            let (pr, slug) = (pr.clone(), slug.to_string());
            watches.push(Watch::Packet { pr, slug, after });
        }
        let kind = label(task, "kind");
        let comments = turn == Some("author") || kind == Some("respond");
        let label = checked(task, "head", commit)?;
        let recorded = recorded_head(home, task.view.id)?;
        let owner_review = turn == Some("owner") && kind == Some("review");
        if owner_review && recorded.is_none() {
            if label.is_some() {
                watches.push(Watch::Unrecorded { pr });
            }
            continue;
        }
        let head = recorded.or(label);
        if comments || owner_review {
            let after = checked(task, "github_at", time)?;
            let repo = repo.to_string();
            watches.push(Watch::Pull {
                pr,
                repo,
                number,
                head,
                after,
                comments,
            });
        }
    }
    assert!(watches.len() <= tasks.len().saturating_mul(2));
    Ok(watches)
}

fn field(program: &'static str, field: String, problem: &'static str) -> WaitError {
    assert!(!field.is_empty() && !problem.is_empty());
    WaitError::Field {
        program,
        field,
        problem,
    }
}

fn list<'v>(
    program: &'static str,
    value: &'v Value,
    name: impl Fn() -> String,
    max: usize,
) -> Result<&'v [Value], WaitError> {
    let list = value.as_array();
    let list = list.ok_or_else(|| field(program, name(), "is missing or not a list"))?;
    if list.len() > max {
        return Err(field(program, name(), "holds too many entries"));
    }
    Ok(list)
}

fn time(program: &'static str, value: &Value, name: String) -> Result<Timestamp, WaitError> {
    let text = value.as_str();
    let text = text.ok_or_else(|| field(program, name.clone(), "is not text"))?;
    text.parse()
        .map_err(|_| field(program, name, "is not an RFC 3339 time"))
}

fn parse(program: &'static str, reply: &[u8]) -> Result<Value, WaitError> {
    let value = serde_json::from_slice(reply);
    let value: Value = value.map_err(|_| field(program, "the reply".into(), "is not JSON"))?;
    assert!(!value.is_null() || reply.trim_ascii() == b"null");
    Ok(value)
}

fn newest_packet_comment(reply: &[u8]) -> Result<Option<Timestamp>, WaitError> {
    let program = "reviews";
    let value = parse(program, reply)?;
    let threads = list(program, &value["threads"], || "threads".into(), THREADS_MAX)?;
    let mut newest: Option<Timestamp> = None;
    for (i, thread) in threads.iter().enumerate() {
        let name = || format!("threads[{i}].comments");
        for (j, comment) in list(program, &thread["comments"], name, COMMENTS_MAX)?
            .iter()
            .enumerate()
        {
            let at = format!("threads[{i}].comments[{j}].inserted_at");
            let at = time(program, &comment["inserted_at"], at)?;
            let kind = format!("threads[{i}].comments[{j}].author.kind");
            match comment["author"]["kind"].as_str() {
                Some("human") => newest = newest.max(Some(at)),
                Some("agent") => {}
                _ => return Err(field(program, kind, "is not human or agent")),
            }
        }
    }
    Ok(newest)
}

fn is_login(login: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"-_[]".contains(&b);
    let valid = !login.is_empty() && login.len() <= LOGIN_BYTES_MAX;
    valid && login.bytes().all(allowed)
}

struct PullState {
    head: String,
    newest: Option<(Timestamp, String)>,
}

fn pull_state(reply: &[u8]) -> Result<PullState, WaitError> {
    let program = "gh";
    let value = parse(program, reply)?;
    let head = value["headRefOid"]
        .as_str()
        .filter(|head| git::is_commit_id(head));
    let head = head.ok_or_else(|| field(program, "headRefOid".into(), "is not a commit id"))?;
    let mut newest: Option<(Timestamp, String)> = None;
    for (name, at_key) in [("comments", "createdAt"), ("reviews", "submittedAt")] {
        let entries = list(program, &value[name], || name.into(), COMMENTS_MAX)?;
        for (i, entry) in entries.iter().enumerate() {
            // A pending review is the viewer's own draft, with a null `submittedAt`.
            let pending = entry["state"].as_str() == Some("PENDING");
            if pending || entry["viewerDidAuthor"].as_bool() == Some(true) {
                continue;
            }
            let at = time(program, &entry[at_key], format!("{name}[{i}].{at_key}"))?;
            let login = entry["author"]["login"].as_str();
            let login_name = format!("{name}[{i}].author.login");
            let login_field = || field(program, login_name, "is not a GitHub login");
            let login = login
                .filter(|login| is_login(login))
                .ok_or_else(login_field)?;
            if newest.as_ref().is_none_or(|(newest, _)| at > *newest) {
                newest = Some((at, login.to_string()));
            }
        }
    }
    let head = head.to_string();
    assert!(git::is_commit_id(&head));
    Ok(PullState { head, newest })
}

fn check(
    sources: &impl Sources,
    watch: &Watch,
    deadline: Instant,
) -> Result<Vec<String>, WaitError> {
    let newer = |at: Timestamp, after: Option<Timestamp>| after.is_none_or(|after| at > after);
    let mut lines = Vec::new();
    match watch {
        Watch::Packet { pr, slug, after } => {
            let newest = newest_packet_comment(&sources.packet(slug, deadline)?)?;
            if let Some(at) = newest.filter(|at| newer(*at, *after)) {
                lines.push(format!("{pr}: packet {slug} has a comment at {at}"));
            }
        }
        Watch::Unrecorded { pr } => lines.push(format!("{pr}: no reviewed head is recorded")),
        Watch::Pull {
            pr,
            repo,
            number,
            head,
            after,
            comments,
        } => {
            let live = pull_state(&sources.pull(repo, *number, deadline)?)?;
            if head.as_ref().is_some_and(|head| *head != live.head) {
                lines.push(format!("{pr}: head moved to {}", live.head));
            }
            let newest = live
                .newest
                .filter(|(at, _)| *comments && newer(*at, *after));
            if let Some((at, login)) = newest {
                lines.push(format!("{pr}: comment by {login} at {at}"));
            }
        }
    }
    assert!(lines.len() <= 2);
    Ok(lines)
}

fn poll(
    sources: &impl Sources,
    watches: &[Watch],
    due: (bool, bool),
    deadline: Instant,
) -> Vec<Result<Vec<String>, WaitError>> {
    let is_due = |watch: &&Watch| match watch {
        Watch::Packet { .. } => due.0,
        Watch::Pull { .. } | Watch::Unrecorded { .. } => due.1,
    };
    let due: Vec<&Watch> = watches.iter().filter(is_due).collect();
    let mut results = Vec::with_capacity(due.len());
    for batch in due.chunks(CALLS_IN_FLIGHT_MAX) {
        thread::scope(|scope| {
            let checks: Vec<_> = (batch.iter())
                .map(|watch| scope.spawn(move || check(sources, watch, deadline)))
                .collect();
            let joined = checks
                .into_iter()
                .map(|check| check.join().unwrap_or(Err(WaitError::PollPanicked)));
            results.extend(joined);
        });
    }
    assert!(results.len() <= watches.len());
    results
}

struct Schedule {
    due: Option<Instant>,
    gap: u64,
    max: u64,
}

impl Schedule {
    fn new(active: bool, start: Instant, min: u64, max: u64) -> Schedule {
        assert!(min > 0 && min <= max);
        let due = active.then_some(start);
        Schedule { due, gap: min, max }
    }

    fn is_due(&self, now: Instant) -> bool {
        self.due.is_some_and(|due| due <= now)
    }

    fn polled(&mut self, at: Instant) {
        assert!(self.due.is_some());
        self.due = Some(at + Duration::from_secs(self.gap));
        self.gap = self.gap.saturating_mul(2).min(self.max);
    }
}

pub struct Outcome {
    pub lines: Vec<String>,
    pub warnings: Vec<WaitError>,
    pub error: Option<WaitError>,
}

fn warn(warnings: &mut Vec<WaitError>, error: WaitError) {
    if warnings.len() < WARNINGS_MAX {
        warnings.push(error);
    }
    assert!(warnings.len() <= WARNINGS_MAX);
}

pub fn wait(sources: &impl Sources, watches: &[Watch], deadline: Instant) -> Outcome {
    let start = Instant::now();
    let has = |packet: bool| (watches.iter()).any(|w| matches!(w, Watch::Packet { .. }) == packet);
    let (min, max) = (PACKET_POLL_SECONDS_MIN, PACKET_POLL_SECONDS_MAX);
    let mut packets = Schedule::new(has(true), start, min, max);
    let (min, max) = (GITHUB_POLL_SECONDS_MIN, GITHUB_POLL_SECONDS_MAX);
    let mut pulls = Schedule::new(has(false), start, min, max);
    let mut warnings = Vec::new();
    for _ in 0..QUEUE_POLLS_MAX {
        let now = Instant::now();
        let due = (packets.is_due(now), pulls.is_due(now));
        if due.0 || due.1 {
            let mut lines = Vec::new();
            let mut malformed = None;
            for result in poll(sources, watches, due, deadline) {
                match result {
                    Ok(found) => lines.extend(found),
                    Err(error) if error.is_malformed() && malformed.is_none() => {
                        malformed = Some(error);
                    }
                    Err(WaitError::Stopped(_)) => {}
                    Err(error) => warn(&mut warnings, error),
                }
            }
            match malformed {
                Some(error) if lines.is_empty() => {
                    let error = Some(error);
                    return Outcome {
                        lines,
                        warnings,
                        error,
                    };
                }
                Some(error) => warn(&mut warnings, error),
                None => {}
            }
            if !lines.is_empty() {
                let error = None;
                return Outcome {
                    lines,
                    warnings,
                    error,
                };
            }
            if due.0 {
                packets.polled(now);
            }
            if due.1 {
                pulls.polled(now);
            }
        }
        let next = [packets.due, pulls.due].into_iter().flatten().min();
        let until = next.filter(|next| *next < deadline).unwrap_or(deadline);
        thread::sleep(until.saturating_duration_since(Instant::now()));
        if until == deadline {
            let (lines, error) = (Vec::new(), None);
            return Outcome {
                lines,
                warnings,
                error,
            };
        }
    }
    let (lines, error) = (Vec::new(), Some(WaitError::TooManyPolls));
    Outcome {
        lines,
        warnings,
        error,
    }
}
