use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::board::{BoardError, ListedTask, TaskId};
use crate::git;

pub const QUEUE_ITEMS_MAX: usize = 200;
pub const CARD_LINES_MAX: usize = 12;
pub const QUEUE_CHANNEL: &str = "queue";
pub const STAND_IN_FILE: &str = "queue.toml";
const STAND_IN_BYTES_MAX: u64 = 64 * 1024;
const REPOS_MAX: usize = 64;
const COLUMNS_MAX: usize = 100;
const VALUE_CHARS_MAX: usize = 24;
const THREADS_SHOWN_MAX: u32 = 9;
const DAYS_SHOWN_MAX: u64 = 99;
const PR_URL: &str = "https://github.com/";
const CARD_PREFIX: &str = "| ";
const HEADER: &str = "  #  KIND     TURN    STATE       AGE  NEXT           PR";
const RANK_WIDTH: usize = 3;
const STATE_WIDTH: usize = 10;
const _: () = assert!(QUEUE_ITEMS_MAX < 1000);

#[derive(Debug)]
pub enum QueueError {
    Read {
        path: PathBuf,
        message: String,
    },
    Field {
        path: PathBuf,
        field: String,
        problem: String,
    },
    Board(BoardError),
    TooManyItems,
    Cycle,
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Read { path, message } => {
                write!(f, "could not read {}: {message}", path.display())
            }
            QueueError::Field {
                path,
                field,
                problem,
            } => write!(f, "{}: {field} {problem}", path.display()),
            QueueError::Board(error) => write!(f, "{error}"),
            QueueError::TooManyItems => {
                write!(f, "the queue holds more than {QUEUE_ITEMS_MAX} items")
            }
            QueueError::Cycle => write!(f, "the queue's needs form a cycle"),
        }
    }
}

impl std::error::Error for QueueError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StandInShape {
    #[serde(default)]
    repo: BTreeMap<String, StandInRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StandInRepo {
    #[serde(default)]
    queue: bool,
}

// The `queue` key is not in rider.toml yet; until it is, it is read from this stand-in file
// with the same `[repo."<path>"]` tables.
pub fn opted_in(home: &Path) -> Result<Vec<PathBuf>, QueueError> {
    assert!(home.is_absolute());
    let path = home.join(STAND_IN_FILE);
    let read_error = |error: std::io::Error| QueueError::Read {
        path: path.clone(),
        message: error.to_string(),
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    };
    let mut text = String::new();
    let bound = STAND_IN_BYTES_MAX.saturating_add(1);
    file.take(bound)
        .read_to_string(&mut text)
        .map_err(read_error)?;
    let field = |field: String, problem: &str| QueueError::Field {
        path: path.clone(),
        field,
        problem: problem.to_string(),
    };
    if u64::try_from(text.len()).map_or(true, |len| len > STAND_IN_BYTES_MAX) {
        return Err(field("the file".into(), "is too large"));
    }
    let shape: StandInShape =
        toml::from_str(&text).map_err(|error| field("the file".into(), &error.to_string()))?;
    if shape.repo.len() > REPOS_MAX {
        return Err(field("repo".into(), "names too many repositories"));
    }
    let mut repos = Vec::with_capacity(shape.repo.len());
    for (key, repo) in shape.repo {
        if !Path::new(&key).is_absolute() {
            return Err(field(format!("repo.{key:?}"), "must be an absolute path"));
        }
        if repo.queue {
            repos.push(PathBuf::from(key));
        }
    }
    assert!(repos.len() <= REPOS_MAX);
    Ok(repos)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Decide,
    Review,
    Respond,
    Unblock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Owner,
    Agents,
    Author,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Owner,
    Direct,
    Mine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrKey {
    repo: String,
    number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    kind: Kind,
    turn: Turn,
    pr: Option<PrKey>,
    source: Option<Source>,
    since: Option<u64>,
    verdict: Option<&'static str>,
    next: Option<&'static str>,
    pin: Option<u32>,
    lines: Option<u64>,
    threads: Option<u32>,
    seen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Invalid {
    Missing(&'static str),
    Malformed {
        key: &'static str,
        value: String,
        allowed: &'static str,
    },
    DuplicatePr {
        value: String,
        first: TaskId,
    },
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Invalid::Missing(key) => write!(f, "label {key} is missing"),
            Invalid::Malformed {
                key,
                value,
                allowed,
            } => write!(f, "label {key} {value:?} is not {allowed}"),
            Invalid::DuplicatePr { value, first } => {
                write!(f, "label pr {value:?} is also on #{}", first.0)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Ranked(usize),
    Stacked,
    Waiting,
}

struct Entry<'a> {
    task: &'a ListedTask,
    item: Result<Item, Invalid>,
}

const KINDS: [(&str, Kind); 3] = [
    ("review", Kind::Review),
    ("respond", Kind::Respond),
    ("unblock", Kind::Unblock),
];
const TURNS: [(&str, Turn); 3] = [
    ("owner", Turn::Owner),
    ("agents", Turn::Agents),
    ("author", Turn::Author),
];
const SOURCES: [(&str, Source); 3] = [
    ("owner", Source::Owner),
    ("direct", Source::Direct),
    ("mine", Source::Mine),
];
const VERDICTS: [(&str, &str); 4] = [
    ("approve", "approve"),
    ("concerns", "concerns"),
    ("blocking", "blocking"),
    ("none", "none"),
];
const NEXTS: [(&str, &str); 7] = [
    ("read-findings", "read-findings"),
    ("approve", "approve"),
    ("post-drafts", "post-drafts"),
    ("reply-threads", "reply-threads"),
    ("ask-rebase", "ask-rebase"),
    ("confirm-order", "confirm-order"),
    ("decide", "decide"),
];

fn label<'a>(task: &'a ListedTask, key: &str) -> Option<&'a str> {
    assert!(!key.is_empty() && !key.contains('='));
    let found = task.view.labels.iter().find(|(known, _)| known == key);
    found.map(|(_, value)| value.as_str())
}

fn malformed(key: &'static str, value: &str, allowed: &'static str) -> Invalid {
    assert!(!key.is_empty() && !allowed.is_empty());
    let value: String = value.chars().take(VALUE_CHARS_MAX).collect();
    assert!(value.chars().count() <= VALUE_CHARS_MAX);
    Invalid::Malformed {
        key,
        value,
        allowed,
    }
}

fn one_of<T: Copy>(
    task: &ListedTask,
    key: &'static str,
    table: &[(&str, T)],
    allowed: &'static str,
) -> Result<Option<T>, Invalid> {
    assert!(!table.is_empty());
    let Some(value) = label(task, key) else {
        return Ok(None);
    };
    let found = table.iter().find(|(name, _)| *name == value);
    assert!(found.is_none_or(|(name, _)| allowed.contains(name)));
    found
        .map(|(_, kind)| Some(*kind))
        .ok_or_else(|| malformed(key, value, allowed))
}

fn number<T: std::str::FromStr>(
    task: &ListedTask,
    key: &'static str,
    allowed: &'static str,
) -> Result<Option<T>, Invalid> {
    let Some(value) = label(task, key) else {
        return Ok(None);
    };
    let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    let parsed = value.parse::<T>().ok().filter(|_| digits);
    assert!(parsed.is_none() || digits);
    parsed
        .map(Some)
        .ok_or_else(|| malformed(key, value, allowed))
}

fn since(task: &ListedTask) -> Result<Option<u64>, Invalid> {
    let Some(value) = label(task, "since") else {
        return Ok(None);
    };
    let parsed = value.parse::<jiff::Timestamp>().ok();
    let millis = parsed.and_then(|at| u64::try_from(at.as_millisecond()).ok());
    assert!(millis.is_none() || parsed.is_some());
    millis
        .map(Some)
        .ok_or_else(|| malformed("since", value, "an RFC 3339 time"))
}

fn pr_key(task: &ListedTask) -> Result<PrKey, Invalid> {
    let value = label(task, "pr").ok_or(Invalid::Missing("pr"))?;
    let bad = || malformed("pr", value, "<owner>/<name>#<n>");
    let (repo, number) = value.split_once('#').ok_or_else(bad)?;
    let (owner, name) = repo.split_once('/').ok_or_else(bad)?;
    let part = |part: &str| {
        let dots = part == "." || part == "..";
        !part.is_empty()
            && !dots
            && (part.bytes()).all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|_| digits && part(owner) && part(name));
    let number = number.ok_or_else(bad)?;
    assert!(repo.is_ascii() && repo.contains('/'));
    Ok(PrKey {
        repo: repo.to_string(),
        number,
    })
}

fn prepared(task: &ListedTask, kind: Kind) -> Result<(), Invalid> {
    let mut required = vec!["head", "main", "since", "verdict", "next", "prep"];
    if kind == Kind::Review {
        required.push("review_task");
    }
    for key in required {
        let value = label(task, key).ok_or(Invalid::Missing(key))?;
        let commit = matches!(key, "head" | "main");
        if commit && !git::is_commit_id(value) {
            return Err(malformed(key, value, "a commit id"));
        }
        if matches!(key, "prep" | "review_task") {
            number::<u64>(task, key, "a task id")?;
        }
    }
    assert!(label(task, "prep").is_some());
    Ok(())
}

fn parse_pr_item(task: &ListedTask) -> Result<Item, Invalid> {
    let pr = pr_key(task)?;
    let kind = one_of(task, "kind", &KINDS, "review, respond or unblock")?;
    let kind = kind.ok_or(Invalid::Missing("kind"))?;
    let turn = one_of(task, "turn", &TURNS, "owner, agents or author")?;
    let turn = turn.ok_or(Invalid::Missing("turn"))?;
    let verdicts = "approve, concerns, blocking or none";
    let verdict = one_of(task, "verdict", &VERDICTS, verdicts)?;
    let nexts = "read-findings, approve, post-drafts, reply-threads, ask-rebase, \
                 confirm-order or decide";
    let next = one_of(task, "next", &NEXTS, nexts)?;
    if turn == Turn::Owner {
        prepared(task, kind)?;
    }
    let item = Item {
        kind,
        turn,
        pr: Some(pr),
        source: one_of(task, "source", &SOURCES, "owner, direct or mine")?,
        since: since(task)?,
        verdict,
        next,
        pin: number(task, "pin", "a number")?,
        lines: number(task, "lines", "a number")?,
        threads: number(task, "threads", "a number")?,
        seen: label(task, "seen").is_some(),
    };
    assert!(item.pr.is_some() && item.kind != Kind::Decide);
    Ok(item)
}

fn decision(task: &ListedTask) -> Result<Item, Invalid> {
    let item = Item {
        kind: Kind::Decide,
        turn: Turn::Owner,
        pr: None,
        source: None,
        since: since(task)?,
        verdict: None,
        next: Some("decide"),
        pin: number(task, "pin", "a number")?,
        lines: None,
        threads: None,
        seen: false,
    };
    assert!(item.pr.is_none() && item.turn == Turn::Owner);
    Ok(item)
}

fn tier(item: &Item) -> u8 {
    assert!(item.turn == Turn::Owner);
    let tier = match (item.kind, item.source) {
        (Kind::Decide, _) => 0,
        (Kind::Unblock, _) => 1,
        (Kind::Review, _) if item.seen => 2,
        (Kind::Respond, _) => 3,
        (Kind::Review, Some(Source::Owner)) => 4,
        (Kind::Review, _) => 5,
    };
    assert!(tier == 0 || item.pr.is_some());
    tier
}

// Places `order`'s entries so each follows every placed entry it needs; `None` on a cycle.
fn place(entries: &[Entry], mut order: Vec<usize>) -> Option<Vec<usize>> {
    assert!(order.len() <= QUEUE_ITEMS_MAX);
    let ids: Vec<TaskId> = order.iter().map(|&at| entries[at].task.view.id).collect();
    let mut placed: Vec<usize> = Vec::with_capacity(order.len());
    for _ in 0..ids.len() {
        let ready = order.iter().position(|&at| {
            let needs = &entries[at].task.view.needs;
            needs.iter().all(|need| {
                !ids.contains(need)
                    || placed
                        .iter()
                        .any(|&done| entries[done].task.view.id == *need)
            })
        })?;
        placed.push(order.remove(ready));
    }
    assert!(order.is_empty() && placed.len() == ids.len());
    Some(placed)
}

fn entries(tasks: &[ListedTask]) -> Vec<Entry<'_>> {
    assert!(tasks.len() <= QUEUE_ITEMS_MAX);
    let mut entries: Vec<Entry> = (tasks.iter())
        .map(|task| Entry {
            task,
            item: if task.view.channel == QUEUE_CHANNEL {
                parse_pr_item(task)
            } else {
                decision(task)
            },
        })
        .collect();
    let keys: Vec<Option<(String, u64)>> = (entries.iter())
        .map(|entry| {
            let pr = entry.item.as_ref().ok().and_then(|item| item.pr.as_ref());
            pr.map(|pr| (pr.repo.to_ascii_lowercase(), pr.number))
        })
        .collect();
    for at in 0..entries.len() {
        let Some(key) = &keys[at] else {
            continue;
        };
        let task = entries[at].task;
        let first = (0..entries.len())
            .filter(|&other| keys[other].as_ref() == Some(key))
            .map(|other| entries[other].task.view.id)
            .filter(|id| id.0 < task.view.id.0)
            .min_by_key(|id| id.0);
        if let Some(first) = first {
            let pr = label(task, "pr").unwrap_or_default();
            let value = pr.chars().take(VALUE_CHARS_MAX).collect();
            entries[at].item = Err(Invalid::DuplicatePr { value, first });
        }
    }
    assert_eq!(entries.len(), tasks.len());
    entries
}

// Owner items that need an open entry not yet on the owner's turn, directly or through
// another such item.
fn stacked(entries: &[Entry]) -> Vec<bool> {
    let owner = |at: usize| {
        entries[at]
            .item
            .as_ref()
            .is_ok_and(|i| i.turn == Turn::Owner)
    };
    let mut stacked = vec![false; entries.len()];
    for _ in 0..=entries.len() {
        let mut changed = false;
        for at in 0..entries.len() {
            if !owner(at) || stacked[at] {
                continue;
            }
            let held = entries[at].task.view.needs.iter().any(|need| {
                let open = entries.iter().position(|other| other.task.view.id == *need);
                open.is_some_and(|other| !owner(other) || stacked[other])
            });
            stacked[at] = held;
            changed |= held;
        }
        if !changed {
            break;
        }
    }
    assert_eq!(stacked.len(), entries.len());
    stacked
}

pub struct View {
    pub lines: Vec<String>,
    pub json: Value,
}

// The queue's lines and JSON from the open tasks in `queue` and `decisions`, at `now`.
pub fn view(tasks: &[ListedTask], now: u64) -> Result<View, QueueError> {
    if tasks.len() > QUEUE_ITEMS_MAX {
        return Err(QueueError::TooManyItems);
    }
    let entries = entries(tasks);
    let valid = |at: &usize| entries[*at].item.is_ok();
    let all: Vec<usize> = (0..entries.len()).filter(valid).collect();
    place(&entries, all).ok_or(QueueError::Cycle)?;
    let held = stacked(&entries);
    let item = |at: usize| entries[at].item.as_ref().ok();
    let with_turn = |turn: Turn, stacked: bool| -> Vec<usize> {
        let found = (0..entries.len())
            .filter(|&at| item(at).is_some_and(|i| i.turn == turn) && held[at] == stacked);
        found.collect()
    };
    let mut owner = with_turn(Turn::Owner, false);
    owner.sort_by_key(|&at| {
        let id = entries[at].task.view.id.0;
        item(at).map(|i| {
            let since = i.since.unwrap_or(u64::MAX);
            let lines = i.lines.unwrap_or(u64::MAX);
            (i.pin.unwrap_or(u32::MAX), tier(i), since, lines, id)
        })
    });
    let ranked = place(&entries, owner).ok_or(QueueError::Cycle)?;
    let mut rest = Vec::new();
    for (turn, stacked) in [
        (Turn::Owner, true),
        (Turn::Agents, false),
        (Turn::Author, false),
    ] {
        let mut group = with_turn(turn, stacked);
        group.sort_by_key(|&at| {
            let since = item(at).and_then(|i| i.since).unwrap_or(u64::MAX);
            (since, entries[at].task.view.id.0)
        });
        rest.extend(group);
    }
    let invalid: Vec<usize> = (0..entries.len()).filter(|at| !valid(at)).collect();
    assert!(ranked.len() + rest.len() + invalid.len() == entries.len());
    Ok(render(&entries, &ranked, &rest, &invalid, now))
}

fn render(
    entries: &[Entry],
    ranked: &[usize],
    rest: &[usize],
    invalid: &[usize],
    now: u64,
) -> View {
    let mut lines = vec![HEADER.to_string()];
    let mut json = Vec::with_capacity(entries.len());
    let listed = (ranked.iter().enumerate()).map(|(rank, &at)| (Place::Ranked(rank + 1), at));
    let others = rest.iter().map(|&at| {
        let owner = entries[at]
            .item
            .as_ref()
            .is_ok_and(|i| i.turn == Turn::Owner);
        (
            if owner {
                Place::Stacked
            } else {
                Place::Waiting
            },
            at,
        )
    });
    for (place, at) in listed.chain(others) {
        let entry = &entries[at];
        let Ok(item) = &entry.item else {
            continue;
        };
        lines.push(row(entry, item, place, now));
        json.push(entry_json(entry, place, None));
    }
    for &at in invalid {
        let entry = &entries[at];
        let problem = entry.item.as_ref().err().map(Invalid::to_string);
        let problem = problem.unwrap_or_default();
        let id = entry.task.view.id.0;
        lines.push(format!("{:>RANK_WIDTH$}  invalid #{id}: {problem}", "!"));
        json.push(entry_json(entry, Place::Waiting, Some(&problem)));
    }
    if let Some(&first) = ranked.first() {
        lines.push(String::new());
        let card = entries[first].task.view.text.lines().take(CARD_LINES_MAX);
        lines.extend(card.map(|line| {
            let clean = line.chars().map(|c| if hidden(c) { '?' } else { c });
            CARD_PREFIX
                .chars()
                .chain(clean.take(COLUMNS_MAX - CARD_PREFIX.len()))
                .collect::<String>()
        }));
    }
    assert!(!lines.is_empty() && json.len() == entries.len());
    View {
        lines,
        json: Value::Array(json),
    }
}

pub fn hidden(c: char) -> bool {
    let format = matches!(
        c,
        '\u{ad}'
            | '\u{61c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
    );
    c.is_control() || format
}

fn row(entry: &Entry, item: &Item, place: Place, now: u64) -> String {
    let rank = match place {
        Place::Ranked(rank) => rank.to_string(),
        Place::Stacked | Place::Waiting => "-".to_string(),
    };
    let kind = match item.kind {
        Kind::Decide => "decide",
        Kind::Review => "review",
        Kind::Respond => "respond",
        Kind::Unblock => "unblock",
    };
    let (turn, state) = match (item.turn, place) {
        (Turn::Owner, Place::Stacked) => ("owner", "stacked".to_string()),
        (Turn::Owner, _) => ("owner", owner_state(item)),
        (Turn::Agents, _) => ("agents", "preparing".to_string()),
        (Turn::Author, _) => ("author", "waiting".to_string()),
    };
    let next = match place {
        Place::Ranked(_) => item.next.unwrap_or("-"),
        Place::Stacked | Place::Waiting => "-",
    };
    let age = item
        .since
        .map_or("-".to_string(), |since| age(now.saturating_sub(since)));
    let link = match &item.pr {
        Some(pr) => format!("{PR_URL}{}/pull/{}", pr.repo, pr.number),
        None => format!("{} #{}", entry.task.view.channel, entry.task.view.id.0),
    };
    assert!(rank.len() <= RANK_WIDTH && age.len() <= 3 && state.len() <= STATE_WIDTH);
    let line = format!(
        "{rank:>RANK_WIDTH$}  {kind:<7}  {turn:<6}  {state:<STATE_WIDTH$}  {age:>3}  \
         {next:<13}  {link}"
    );
    assert_eq!(line.len() - link.len(), HEADER.len() - "PR".len());
    line
}

fn owner_state(item: &Item) -> String {
    let state = match item.kind {
        Kind::Decide => "open".to_string(),
        Kind::Unblock => "conflict".to_string(),
        Kind::Respond => match item.threads {
            Some(count) if count > THREADS_SHOWN_MAX => format!("{THREADS_SHOWN_MAX}+ threads"),
            Some(count) => format!("{count} threads"),
            None => "-".to_string(),
        },
        Kind::Review => item.verdict.unwrap_or("-").to_string(),
    };
    assert!(!state.is_empty() && state.len() <= STATE_WIDTH);
    state
}

fn age(millis: u64) -> String {
    let seconds = millis / 1000;
    let age = match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", (seconds / 86_400).min(DAYS_SHOWN_MAX)),
    };
    assert!(age.len() <= 3);
    age
}

fn entry_json(entry: &Entry, place: Place, invalid: Option<&str>) -> Value {
    let labels: serde_json::Map<String, Value> = (entry.task.view.labels.iter())
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    let needs: Vec<u64> = entry.task.view.needs.iter().map(|need| need.0).collect();
    let rank = match place {
        Place::Ranked(rank) => Some(rank),
        Place::Stacked | Place::Waiting => None,
    };
    assert!(labels.len() == entry.task.view.labels.len());
    json!({
        "id": entry.task.view.id.0,
        "channel": entry.task.view.channel,
        "rank": rank,
        "labels": labels,
        "needs": needs,
        "invalid": invalid,
    })
}
