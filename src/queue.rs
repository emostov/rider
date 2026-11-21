use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::board::{BoardError, ListedTask, TaskId};

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
const HEADER: &str = " #  KIND     TURN    STATE      AGE  NEXT           PR";

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
    verdict: Option<String>,
    next: Option<String>,
    pin: Option<u32>,
    lines: Option<u64>,
    threads: Option<u32>,
    seen: bool,
}

struct Entry<'a> {
    task: &'a ListedTask,
    item: Result<Item, String>,
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
const VERDICTS: [&str; 4] = ["approve", "concerns", "blocking", "none"];
const NEXTS: [&str; 7] = [
    "read-findings",
    "approve",
    "post-drafts",
    "reply-threads",
    "ask-rebase",
    "confirm-order",
    "decide",
];

fn label<'a>(task: &'a ListedTask, key: &str) -> Option<&'a str> {
    assert!(!key.is_empty() && !key.contains('='));
    let found = task.view.labels.iter().find(|(known, _)| known == key);
    found.map(|(_, value)| value.as_str())
}

fn bad_value(key: &str, value: &str, allowed: &str) -> String {
    assert!(!key.is_empty() && !allowed.is_empty());
    let shown: String = value.chars().take(VALUE_CHARS_MAX).collect();
    format!("label {key} {shown:?} is not {allowed}")
}

fn one_of<T: Copy>(task: &ListedTask, key: &str, table: &[(&str, T)]) -> Result<Option<T>, String> {
    let Some(value) = label(task, key) else {
        return Ok(None);
    };
    let found = table.iter().find(|(name, _)| *name == value);
    let names: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
    let allowed = match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => names.join(""),
    };
    found
        .map(|(_, kind)| Some(*kind))
        .ok_or_else(|| bad_value(key, value, &allowed))
}

fn number<T: std::str::FromStr>(task: &ListedTask, key: &str) -> Result<Option<T>, String> {
    let Some(value) = label(task, key) else {
        return Ok(None);
    };
    let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    let parsed = value.parse::<T>().ok().filter(|_| digits);
    parsed
        .map(Some)
        .ok_or_else(|| bad_value(key, value, "a number"))
}

fn since(task: &ListedTask) -> Result<Option<u64>, String> {
    let Some(value) = label(task, "since") else {
        return Ok(None);
    };
    let parsed = value.parse::<jiff::Timestamp>().ok();
    let millis = parsed.and_then(|at| u64::try_from(at.as_millisecond()).ok());
    millis
        .map(Some)
        .ok_or_else(|| bad_value("since", value, "an RFC 3339 time"))
}

fn pr_key(task: &ListedTask) -> Result<PrKey, String> {
    let value = label(task, "pr").ok_or("label pr is missing")?;
    let bad = || bad_value("pr", value, "<owner>/<name>#<n>");
    let (repo, number) = value.split_once('#').ok_or_else(bad)?;
    let (owner, name) = repo.split_once('/').ok_or_else(bad)?;
    let part = |part: &str| {
        !part.is_empty() && (part.bytes()).all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|_| digits && part(owner) && part(name));
    let number = number.ok_or_else(bad)?;
    Ok(PrKey {
        repo: repo.to_string(),
        number,
    })
}

fn checked(value: Option<&str>, key: &str, allowed: &[&str]) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(value) if allowed.contains(&value) => Ok(Some(value.to_string())),
        Some(value) => Err(bad_value(key, value, &allowed.join(", "))),
    }
}

fn parse_pr_item(task: &ListedTask) -> Result<Item, String> {
    let pr = pr_key(task)?;
    let kind = one_of(task, "kind", &KINDS)?.ok_or("label kind is missing")?;
    let turn = one_of(task, "turn", &TURNS)?.ok_or("label turn is missing")?;
    let verdict = checked(label(task, "verdict"), "verdict", &VERDICTS)?;
    let next = checked(label(task, "next"), "next", &NEXTS)?;
    if turn == Turn::Owner {
        let mut required = vec!["head", "main", "since", "verdict", "next", "prep"];
        if kind == Kind::Review {
            required.push("review_task");
        }
        if let Some(key) = required.into_iter().find(|key| label(task, key).is_none()) {
            return Err(format!("label {key} is missing"));
        }
    }
    let item = Item {
        kind,
        turn,
        pr: Some(pr),
        source: one_of(task, "source", &SOURCES)?,
        since: since(task)?,
        verdict,
        next,
        pin: number(task, "pin")?,
        lines: number(task, "lines")?,
        threads: number(task, "threads")?,
        seen: label(task, "seen").is_some(),
    };
    assert!(item.pr.is_some() && item.kind != Kind::Decide);
    Ok(item)
}

fn decision(task: &ListedTask) -> Result<Item, String> {
    Ok(Item {
        kind: Kind::Decide,
        turn: Turn::Owner,
        pr: None,
        source: None,
        since: since(task)?,
        verdict: None,
        next: Some("decide".to_string()),
        pin: number(task, "pin")?,
        lines: None,
        threads: None,
        seen: false,
    })
}

fn tier(item: &Item) -> u8 {
    match (item.kind, item.source) {
        (Kind::Decide, _) => 0,
        (Kind::Unblock, _) => 1,
        (Kind::Review, _) if item.seen => 2,
        (Kind::Respond, _) => 3,
        (Kind::Review, Some(Source::Owner)) => 4,
        (Kind::Review, _) => 5,
    }
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

pub struct View {
    pub lines: Vec<String>,
    pub json: Value,
}

// The queue's lines and JSON from the open tasks in `queue` and `decisions`, at `now`.
pub fn view(tasks: &[ListedTask], now: u64) -> Result<View, QueueError> {
    if tasks.len() > QUEUE_ITEMS_MAX {
        return Err(QueueError::TooManyItems);
    }
    let entries: Vec<Entry> = (tasks.iter())
        .map(|task| Entry {
            task,
            item: if task.channel == QUEUE_CHANNEL {
                parse_pr_item(task)
            } else {
                decision(task)
            },
        })
        .collect();
    let valid = |at: &usize| entries[*at].item.is_ok();
    let all: Vec<usize> = (0..entries.len()).filter(valid).collect();
    place(&entries, all).ok_or(QueueError::Cycle)?;
    let item = |at: usize| entries[at].item.as_ref().ok();
    let with_turn = |turn: Turn| -> Vec<usize> {
        let found = (0..entries.len()).filter(|&at| item(at).is_some_and(|i| i.turn == turn));
        found.collect()
    };
    let mut owner = with_turn(Turn::Owner);
    owner.sort_by_key(|&at| {
        let i = item(at);
        let id = entries[at].task.view.id.0;
        i.map(|i| {
            (
                i.pin.unwrap_or(u32::MAX),
                tier(i),
                i.since.unwrap_or(u64::MAX),
                i.lines.unwrap_or(u64::MAX),
                id,
            )
        })
    });
    let ranked = place(&entries, owner).ok_or(QueueError::Cycle)?;
    let mut rest = Vec::new();
    for turn in [Turn::Agents, Turn::Author] {
        let mut group = with_turn(turn);
        group.sort_by_key(|&at| {
            (
                item(at).and_then(|i| i.since).unwrap_or(u64::MAX),
                entries[at].task.view.id.0,
            )
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
    let listed = ranked
        .iter()
        .enumerate()
        .map(|(rank, &at)| (Some(rank + 1), at));
    for (rank, at) in listed.chain(rest.iter().map(|&at| (None, at))) {
        let entry = &entries[at];
        let Ok(item) = &entry.item else {
            continue;
        };
        lines.push(row(entry, item, rank, now));
        json.push(entry_json(entry, rank, None));
    }
    for &at in invalid {
        let entry = &entries[at];
        let problem = entry.item.as_ref().err().map_or("", String::as_str);
        lines.push(format!(" !  invalid #{}: {problem}", entry.task.view.id.0));
        json.push(entry_json(entry, None, Some(problem)));
    }
    if let Some(&first) = ranked.first() {
        lines.push(String::new());
        let card = entries[first].task.view.text.lines().take(CARD_LINES_MAX);
        lines.extend(card.map(|line| {
            let clean = line.chars().map(|c| if c.is_control() { '?' } else { c });
            clean.take(COLUMNS_MAX).collect::<String>()
        }));
    }
    assert!(!lines.is_empty() && json.len() <= entries.len());
    View {
        lines,
        json: Value::Array(json),
    }
}

fn row(entry: &Entry, item: &Item, rank: Option<usize>, now: u64) -> String {
    let rank = rank.map_or("-".to_string(), |rank| rank.to_string());
    let kind = match item.kind {
        Kind::Decide => "decide",
        Kind::Review => "review",
        Kind::Respond => "respond",
        Kind::Unblock => "unblock",
    };
    let (turn, state) = match item.turn {
        Turn::Owner => ("owner", owner_state(item)),
        Turn::Agents => ("agents", "preparing".to_string()),
        Turn::Author => ("author", "waiting".to_string()),
    };
    let next = match item.turn {
        Turn::Owner => item.next.as_deref().unwrap_or("-"),
        Turn::Agents | Turn::Author => "-",
    };
    let age = item
        .since
        .map_or("-".to_string(), |since| age(now.saturating_sub(since)));
    let link = match &item.pr {
        Some(pr) => format!("{PR_URL}{}/pull/{}", pr.repo, pr.number),
        None => format!("{} #{}", entry.task.channel, entry.task.view.id.0),
    };
    assert!(age.len() <= 3 && !state.is_empty());
    let line = format!("{rank:>2}  {kind:<7}  {turn:<6}  {state:<9}  {age:>3}  {next:<13}  {link}");
    assert!(line.ends_with(&link));
    line
}

fn owner_state(item: &Item) -> String {
    match item.kind {
        Kind::Decide => "open".to_string(),
        Kind::Unblock => "conflict".to_string(),
        Kind::Respond => match item.threads {
            Some(count) if count > THREADS_SHOWN_MAX => format!("{THREADS_SHOWN_MAX}+ threads"),
            Some(count) => format!("{count} threads"),
            None => "-".to_string(),
        },
        Kind::Review => item.verdict.clone().unwrap_or_else(|| "-".to_string()),
    }
}

fn age(millis: u64) -> String {
    let seconds = millis / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", (seconds / 86_400).min(DAYS_SHOWN_MAX)),
    }
}

fn entry_json(entry: &Entry, rank: Option<usize>, invalid: Option<&str>) -> Value {
    let labels: serde_json::Map<String, Value> = (entry.task.view.labels.iter())
        .filter(|(key, _)| !key.starts_with("board."))
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    let needs: Vec<u64> = entry.task.view.needs.iter().map(|need| need.0).collect();
    json!({
        "id": entry.task.view.id.0,
        "channel": entry.task.channel,
        "rank": rank,
        "labels": labels,
        "needs": needs,
        "invalid": invalid,
    })
}
