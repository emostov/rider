use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::Value;

use crate::board::{ListedTask, TaskId};
use crate::git;
use crate::queue::QUEUE_ITEMS_MAX;

pub const PACKET_POLL_SECONDS_MIN: u64 = 10;
pub const PACKET_POLL_SECONDS_MAX: u64 = 60;
pub const GITHUB_POLL_SECONDS_MIN: u64 = 60;
pub const GITHUB_POLL_SECONDS_MAX: u64 = 300;
const QUEUE_POLLS_MAX: u32 = 4096;
const REPLY_BYTES_MAX: u64 = 16 * 1024 * 1024;
const THREADS_MAX: usize = 1024;
const COMMENTS_MAX: usize = 4096;
const SLUG_BYTES_MAX: usize = 64;
const LABEL_SHOWN_CHARS_MAX: usize = 48;

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
    TooManyPolls,
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
            WaitError::TooLarge(program) => {
                write!(f, "{program} printed more than {REPLY_BYTES_MAX} bytes")
            }
            WaitError::Field {
                program,
                field,
                problem,
            } => write!(f, "{program}'s reply: {field} {problem}"),
            WaitError::Label { task, key, value } => {
                write!(
                    f,
                    "queue task #{}: label {key}={value:?} is malformed",
                    task.0
                )
            }
            WaitError::TooManyPolls => write!(f, "the wait polled {QUEUE_POLLS_MAX} times"),
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
        head: Option<String>,
        after: Option<Timestamp>,
    },
}

pub trait Sources: Sync {
    fn packet(&self, slug: &str) -> Result<Vec<u8>, WaitError>;
    fn pull(&self, repo: &str, number: &str) -> Result<Vec<u8>, WaitError>;
}

pub struct Cli;

impl Sources for Cli {
    // `reviews threads` prints neither comment times nor replies, so the packet's comments
    // are read from `reviews show`; a `reviews wait --follow` would replace this one call.
    fn packet(&self, slug: &str) -> Result<Vec<u8>, WaitError> {
        assert!(!slug.starts_with('-'));
        run("reviews", &["show", slug])
    }

    fn pull(&self, repo: &str, number: &str) -> Result<Vec<u8>, WaitError> {
        assert!(!repo.starts_with('-') && !number.starts_with('-'));
        let fields = "headRefOid,comments,reviews";
        run(
            "gh",
            &["pr", "view", number, "--repo", repo, "--json", fields],
        )
    }
}

fn run(program: &'static str, args: &[&str]) -> Result<Vec<u8>, WaitError> {
    assert!(!args.is_empty());
    let spawn_error = |error: std::io::Error| WaitError::Spawn {
        program,
        message: error.to_string(),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(spawn_error)?;
    let mut out = Vec::new();
    let read = match child.stdout.take() {
        Some(stdout) => stdout.take(REPLY_BYTES_MAX + 1).read_to_end(&mut out),
        None => Ok(0),
    };
    let status = child.wait().map_err(spawn_error)?;
    read.map_err(spawn_error)?;
    if u64::try_from(out.len()).map_or(true, |len| len > REPLY_BYTES_MAX) {
        return Err(WaitError::TooLarge(program));
    }
    if !status.success() {
        let code = status.code();
        return Err(WaitError::Exit { program, code });
    }
    assert!(out.len() as u64 <= REPLY_BYTES_MAX);
    Ok(out)
}

fn label<'a>(task: &'a ListedTask, key: &str) -> Option<&'a str> {
    let found = task.view.labels.iter().find(|(known, _)| known == key);
    found.map(|(_, value)| value.as_str())
}

fn checked<'a>(
    task: &'a ListedTask,
    key: &'static str,
    valid: impl Fn(&str) -> bool,
) -> Result<Option<&'a str>, WaitError> {
    let Some(value) = label(task, key) else {
        return Ok(None);
    };
    if valid(value) {
        return Ok(Some(value));
    }
    let value = value.chars().take(LABEL_SHOWN_CHARS_MAX).collect();
    Err(WaitError::Label {
        task: task.view.id,
        key,
        value,
    })
}

fn time(task: &ListedTask, key: &'static str) -> Result<Option<Timestamp>, WaitError> {
    let value = checked(task, key, |value| value.parse::<Timestamp>().is_ok())?;
    Ok(value.and_then(|value| value.parse().ok()))
}

fn pr_valid(value: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let Some((repo, number)) = value.split_once('#') else {
        return false;
    };
    let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    digits
        && repo
            .split_once('/')
            .is_some_and(|(owner, name)| part(owner) && part(name))
}

pub fn watches(tasks: &[ListedTask]) -> Result<Vec<Watch>, WaitError> {
    if tasks.len() > QUEUE_ITEMS_MAX {
        return Err(WaitError::TooManyPolls);
    }
    let mut watches = Vec::with_capacity(tasks.len().saturating_mul(2));
    for task in tasks {
        let Some(pr) = checked(task, "pr", pr_valid)? else {
            continue;
        };
        let turn = label(task, "turn");
        let slug_valid = |slug: &str| {
            !slug.is_empty()
                && slug.len() <= SLUG_BYTES_MAX
                && slug
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                && !slug.starts_with('-')
        };
        let slug = checked(task, "packet", slug_valid)?;
        if let (Some(slug), Some("owner")) = (slug, turn) {
            let after = time(task, "packet_at")?;
            let (pr, slug) = (pr.to_string(), slug.to_string());
            watches.push(Watch::Packet { pr, slug, after });
        }
        if turn == Some("author") || label(task, "kind") == Some("respond") {
            let head = checked(task, "head", git::is_commit_id)?.map(String::from);
            let after = time(task, "github_at")?;
            let pr = pr.to_string();
            watches.push(Watch::Pull { pr, head, after });
        }
    }
    assert!(watches.len() <= tasks.len().saturating_mul(2));
    Ok(watches)
}

struct Fields<'a> {
    program: &'static str,
    value: &'a Value,
}

impl Fields<'_> {
    fn error(&self, field: String, problem: &'static str) -> WaitError {
        let program = self.program;
        WaitError::Field {
            program,
            field,
            problem,
        }
    }

    fn list<'v>(
        &self,
        value: &'v Value,
        field: &str,
        max: usize,
    ) -> Result<&'v [Value], WaitError> {
        let list = value.get(field).and_then(Value::as_array);
        let list = list.ok_or_else(|| self.error(field.to_string(), "is missing or not a list"))?;
        if list.len() > max {
            return Err(self.error(field.to_string(), "holds too many entries"));
        }
        Ok(list)
    }

    fn time(&self, value: &Value, field: String) -> Result<Timestamp, WaitError> {
        let text = value
            .as_str()
            .ok_or_else(|| self.error(field.clone(), "is not text"))?;
        text.parse()
            .map_err(|_| self.error(field, "is not an RFC 3339 time"))
    }
}

fn parse(program: &'static str, reply: &[u8]) -> Result<Value, WaitError> {
    let value: Value = serde_json::from_slice(reply).map_err(|_| WaitError::Field {
        program,
        field: "the reply".to_string(),
        problem: "is not JSON",
    })?;
    assert!(!value.is_null() || reply.trim_ascii() == b"null");
    Ok(value)
}

fn newest_packet_comment(reply: &[u8]) -> Result<Option<Timestamp>, WaitError> {
    let value = parse("reviews", reply)?;
    let fields = Fields {
        program: "reviews",
        value: &value,
    };
    let mut newest: Option<Timestamp> = None;
    for (i, thread) in fields
        .list(fields.value, "threads", THREADS_MAX)?
        .iter()
        .enumerate()
    {
        let comments = fields.list(thread, "comments", COMMENTS_MAX).map_err(|_| {
            fields.error(format!("threads[{i}].comments"), "is missing or not a list")
        })?;
        for (j, comment) in comments.iter().enumerate() {
            let field = format!("threads[{i}].comments[{j}].inserted_at");
            let at = fields.time(&comment["inserted_at"], field)?;
            newest = newest.max(Some(at));
        }
    }
    Ok(newest)
}

fn pull_state(reply: &[u8]) -> Result<(String, Option<(Timestamp, String)>), WaitError> {
    let value = parse("gh", reply)?;
    let fields = Fields {
        program: "gh",
        value: &value,
    };
    let head = value["headRefOid"]
        .as_str()
        .filter(|head| git::is_commit_id(head));
    let head = head.ok_or_else(|| fields.error("headRefOid".into(), "is not a commit id"))?;
    let mut newest: Option<(Timestamp, String)> = None;
    for (list, at_key) in [("comments", "createdAt"), ("reviews", "submittedAt")] {
        for (i, entry) in fields
            .list(fields.value, list, COMMENTS_MAX)?
            .iter()
            .enumerate()
        {
            let at = fields.time(&entry[at_key], format!("{list}[{i}].{at_key}"))?;
            let login = entry["author"]["login"].as_str();
            let field = format!("{list}[{i}].author.login");
            let login = login.ok_or_else(|| fields.error(field, "is not text"))?;
            if newest.as_ref().is_none_or(|(newest, _)| at > *newest) {
                newest = Some((at, login.to_string()));
            }
        }
    }
    Ok((head.to_string(), newest))
}

fn check(sources: &impl Sources, watch: &Watch) -> Result<Vec<String>, WaitError> {
    let newer = |at: Timestamp, after: Option<Timestamp>| after.is_none_or(|after| at > after);
    let mut lines = Vec::new();
    match watch {
        Watch::Packet { pr, slug, after } => {
            let newest = newest_packet_comment(&sources.packet(slug)?)?;
            if let Some(at) = newest.filter(|at| newer(*at, *after)) {
                lines.push(format!("{pr}: packet {slug} has a comment at {at}"));
            }
        }
        Watch::Pull { pr, head, after } => {
            let (repo, number) = pr.split_once('#').unwrap_or((pr, ""));
            let (live, newest) = pull_state(&sources.pull(repo, number)?)?;
            if head.as_ref().is_some_and(|head| *head != live) {
                lines.push(format!("{pr}: head moved to {live}"));
            }
            if let Some((at, login)) = newest.filter(|(at, _)| newer(*at, *after)) {
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
    packets: bool,
    pulls: bool,
) -> Result<Vec<String>, WaitError> {
    let due = |watch: &&Watch| match watch {
        Watch::Packet { .. } => packets,
        Watch::Pull { .. } => pulls,
    };
    let results = thread::scope(|scope| {
        let checks: Vec<_> = (watches.iter().filter(due))
            .map(|watch| scope.spawn(move || check(sources, watch)))
            .collect();
        let joined = checks.into_iter().map(|check| {
            let program = "a poll";
            let panicked = || {
                Err(WaitError::Exit {
                    program,
                    code: None,
                })
            };
            check.join().unwrap_or_else(|_| panicked())
        });
        joined.collect::<Vec<_>>()
    });
    let mut lines = Vec::new();
    for result in results {
        lines.extend(result?);
    }
    assert!(lines.len() <= watches.len().saturating_mul(2));
    Ok(lines)
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

// The change lines once something differs from the labels; none at `deadline`.
pub fn wait(
    sources: &impl Sources,
    watches: &[Watch],
    deadline: Instant,
) -> Result<Vec<String>, WaitError> {
    let start = Instant::now();
    let has = |packet: bool| (watches.iter()).any(|w| matches!(w, Watch::Packet { .. }) == packet);
    let (min, max) = (PACKET_POLL_SECONDS_MIN, PACKET_POLL_SECONDS_MAX);
    let mut packets = Schedule::new(has(true), start, min, max);
    let (min, max) = (GITHUB_POLL_SECONDS_MIN, GITHUB_POLL_SECONDS_MAX);
    let mut pulls = Schedule::new(has(false), start, min, max);
    for _ in 0..QUEUE_POLLS_MAX {
        let now = Instant::now();
        let (packets_due, pulls_due) = (packets.is_due(now), pulls.is_due(now));
        if packets_due || pulls_due {
            let lines = poll(sources, watches, packets_due, pulls_due)?;
            if !lines.is_empty() {
                return Ok(lines);
            }
            if packets_due {
                packets.polled(now);
            }
            if pulls_due {
                pulls.polled(now);
            }
        }
        let next = [packets.due, pulls.due].into_iter().flatten().min();
        let until = next.filter(|next| *next < deadline).unwrap_or(deadline);
        thread::sleep(until.saturating_duration_since(Instant::now()));
        if until == deadline {
            return Ok(Vec::new());
        }
    }
    Err(WaitError::TooManyPolls)
}
