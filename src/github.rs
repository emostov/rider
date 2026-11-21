use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::board::{Board, ListedTask, NewTask, TaskId, TaskState};
use crate::brief::label;
use crate::git::{self, ENV_REMOVED};
use crate::roles::ROLES;
use crate::run_task::{RESUMABLE_ROLES, session_path};
use crate::worktree::BRANCH_LABEL;

pub const SCHEMA: &str = "rider-github@1";
pub const STATE_FILE: &str = "github.json";
pub const WATCHES_MAX: usize = 64;
pub const SEEN_MAX: usize = 4096;
pub const EXCERPT_BYTES_MAX: usize = 300;
pub const POLL_TICKS: u64 = 12;
pub const BACKOFF_MAX: u64 = 16;
const ITEMS_MAX: usize = 200;
const STATE_BYTES_MAX: u64 = 512 * 1024;
const GH_SECONDS_MAX: u64 = 60;
const ROUNDS_MAX: u64 = u64::MAX;
pub const FIX_ROUNDS_MAX: usize = 2;
const LOG_TAIL_BYTES_MAX: usize = 2000;
const NEVER_GITHUB: &str = "Never reply on GitHub.";
const PR_FIELDS: &str = "number,url,headRefOid,state,statusCheckRollup,reviews,comments";
const RATE_LIMITED: [&str; 2] = ["rate limit", "API rate"];

#[derive(Debug)]
pub enum WatchError {
    Gh { args: String, detail: String },
    RateLimited,
    Parse { what: &'static str, detail: String },
    TooMany { what: &'static str, max: usize },
    State(String),
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WatchError::Gh { args, detail } => write!(f, "gh {args}: {detail}"),
            WatchError::RateLimited => write!(f, "gh reports rate limiting"),
            WatchError::Parse { what, detail } => write!(f, "could not parse {what}: {detail}"),
            WatchError::TooMany { what, max } => write!(f, "more than {max} {what}"),
            WatchError::State(detail) => write!(f, "github state: {detail}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePr {
    number: u64,
    url: String,
    head_ref_oid: String,
    state: String,
    #[serde(default)]
    status_check_rollup: Vec<WireCheck>,
    #[serde(default)]
    reviews: Vec<WireReview>,
    #[serde(default)]
    comments: Vec<WireComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    details_url: String,
}

#[derive(Deserialize)]
struct WireAuthor {
    login: String,
}

#[derive(Deserialize)]
struct WireReview {
    id: String,
    author: WireAuthor,
    body: String,
    state: String,
}

#[derive(Deserialize)]
struct WireComment {
    id: String,
    author: WireAuthor,
    body: String,
    url: String,
}

#[derive(Deserialize)]
struct WireInline {
    id: u64,
    user: WireAuthor,
    body: String,
    path: Option<String>,
    line: Option<u64>,
    html_url: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Seen {
    keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checks {
    Pending,
    Passed,
    Failed,
}

struct Watched<'a> {
    task: TaskId,
    channel: &'a str,
    role: &'a str,
    repo: &'a Path,
    branch: &'a str,
    resumable: bool,
}

struct Change {
    key: String,
    record: Value,
    fix: Option<Fix>,
}

enum Fix {
    Ci,
    Change(String),
}

pub fn start(
    board: Box<dyn Board + Send>,
    home: PathBuf,
    tick_millis: u64,
    log: Box<dyn Fn(&str) + Send>,
) -> Result<(), String> {
    assert!(home.is_absolute() && tick_millis > 0);
    let builder = thread::Builder::new().name("github".into());
    let spawned = builder.spawn(move || watch(board.as_ref(), &home, tick_millis, &*log));
    spawned.map(|_| ()).map_err(|error| error.to_string())
}

fn watch(board: &dyn Board, home: &Path, tick_millis: u64, log: &dyn Fn(&str)) {
    let mut backoff: u64 = 1;
    for _ in 0..ROUNDS_MAX {
        let wait = tick_millis
            .saturating_mul(POLL_TICKS)
            .saturating_mul(backoff);
        thread::sleep(Duration::from_millis(wait));
        backoff = match round(board, home, log) {
            Ok(0) => backoff.saturating_mul(2).min(BACKOFF_MAX),
            Ok(_) => 1,
            Err(error) => {
                log(&format!("github watch: {error}"));
                backoff.saturating_mul(2).min(BACKOFF_MAX)
            }
        };
        assert!((1..=BACKOFF_MAX).contains(&backoff));
    }
}

fn round(board: &dyn Board, home: &Path, log: &dyn Fn(&str)) -> Result<usize, WatchError> {
    let state_path = home.join(STATE_FILE);
    let mut seen = read_seen(&state_path)?;
    let roles = ROLES.map(|role| role.label);
    let listing = board
        .list_labelled(crate::daemon::SUBTREE, "role", &roles)
        .map_err(|error| WatchError::State(error.to_string()))?;
    let mut tasks: Vec<&ListedTask> = listing.tasks.iter().collect();
    tasks.sort_by_key(|task| std::cmp::Reverse(task.view.id.0));
    let mut branches: Vec<&str> = Vec::new();
    let mut posted = 0usize;
    for listed in tasks {
        let view = &listed.view;
        let (Some(repo), Some(branch), Some(role)) = (
            label(view, "repo"),
            label(view, BRANCH_LABEL),
            label(view, "role"),
        ) else {
            continue;
        };
        if branches.contains(&branch) {
            continue;
        }
        if branches.len() >= WATCHES_MAX {
            break;
        }
        branches.push(branch);
        let task = view.id;
        let running = matches!(view.state, TaskState::Ready | TaskState::Claimed);
        if running || seen.keys.contains(&ended_key(task)) || !git::is_safe_ref(branch) {
            continue;
        }
        let watched = Watched {
            task,
            channel: &listed.channel,
            role,
            repo: Path::new(repo),
            branch,
            resumable: RESUMABLE_ROLES.contains(&role) && session_path(home, task).is_file(),
        };
        if !watched.repo.is_absolute() {
            continue;
        }
        match watch_one(board, &watched, &mut seen) {
            Ok(count) => posted = posted.saturating_add(count),
            Err(WatchError::RateLimited) => {
                write_seen(&state_path, &seen)?;
                return Err(WatchError::RateLimited);
            }
            Err(error) => log(&format!("github watch #{}: {error}", task.0)),
        }
        write_seen(&state_path, &seen)?;
    }
    assert!(branches.len() <= WATCHES_MAX);
    Ok(posted)
}

fn ended_key(task: TaskId) -> String {
    format!("ended:{}", task.0)
}

fn watch_one(board: &dyn Board, watched: &Watched, seen: &mut Seen) -> Result<usize, WatchError> {
    let args = [
        "pr",
        "list",
        "--head",
        watched.branch,
        "--state",
        "all",
        "--limit",
        "1",
        "--json",
        PR_FIELDS,
    ];
    let stdout = gh(watched.repo, &args)?;
    let prs: Vec<WirePr> = parse(&stdout, "gh pr list")?;
    let Some(pr) = prs.into_iter().next() else {
        return Ok(0);
    };
    if pr.state != "OPEN" {
        remember(seen, ended_key(watched.task));
        return Ok(0);
    }
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{}/comments", pr.number);
    let inline: Vec<WireInline> = parse(&gh(watched.repo, &["api", &path])?, "gh api")?;
    let mut posted = 0usize;
    for change in changes(&pr, &inline)? {
        if seen.keys.contains(&change.key) {
            continue;
        }
        let Change {
            key,
            mut record,
            fix,
        } = change;
        match fix.filter(|_| watched.resumable) {
            Some(fix) if fix_rounds(seen, watched.branch, &pr.head_ref_oid) < FIX_ROUNDS_MAX => {
                let fix = queue_fix(board, watched, seen, &key, fix, &pr)?;
                record["route"] = json!("owner");
                record["task"] = json!(fix.0);
            }
            _ => record["route"] = json!("lead"),
        }
        board
            .post_structured(watched.task, SCHEMA, &record.to_string())
            .map_err(|error| WatchError::State(error.to_string()))?;
        remember(seen, key);
        posted = posted.saturating_add(1);
    }
    Ok(posted)
}

fn fix_rounds(seen: &Seen, branch: &str, head: &str) -> usize {
    let prefix = format!("fix:{branch}:{head}:");
    seen.keys
        .iter()
        .filter(|key| key.starts_with(&prefix))
        .count()
}

fn queue_fix(
    board: &dyn Board,
    watched: &Watched,
    seen: &mut Seen,
    key: &str,
    fix: Fix,
    pr: &WirePr,
) -> Result<TaskId, WatchError> {
    assert!(watched.resumable && !key.is_empty());
    let prefix = format!("fix:{}:", watched.branch);
    let after = seen
        .keys
        .iter()
        .rev()
        .find_map(|seen| seen.strip_prefix(&prefix)?.rsplit(':').next()?.parse().ok())
        .map(TaskId);
    let text = match fix {
        Fix::Ci => ci_brief(watched, pr),
        Fix::Change(brief) => brief,
    };
    let onto = watched.task.0.to_string();
    let repo = watched.repo.to_string_lossy();
    let labels = [
        ("repo", repo.as_ref()),
        ("onto", onto.as_str()),
        ("resume", "1"),
        ("role", watched.role),
    ];
    let idempotency = format!("rider-github-{key}");
    let task = NewTask {
        channel: watched.channel,
        text: &text,
        labels: &labels,
        needs: after,
        key: &idempotency,
    };
    let fix = board
        .create_task(&task)
        .map_err(|error| WatchError::State(error.to_string()))?;
    remember(seen, format!("{prefix}{}:{}", pr.head_ref_oid, fix.0));
    Ok(fix)
}

fn ci_brief(watched: &Watched, pr: &WirePr) -> String {
    let failed = pr
        .status_check_rollup
        .iter()
        .find(|check| is_failure(check));
    let (job, url) = failed.map_or(("", ""), |check| (check.name.as_str(), &check.details_url));
    let tail = match url.rsplit_once("/job/") {
        Some((_, id)) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => {
            let args = ["run", "view", "--job", id, "--log-failed"];
            gh(watched.repo, &args).unwrap_or_else(|error| format!("(no log: {error})"))
        }
        _ => "(no job log)".to_string(),
    };
    format!(
        "CI failed on head {head} of pull request {pr_url}: job `{job}`, {url}.\n\nThe end \
         of its log:\n\n```\n{tail}\n```\n\nFix the failure and commit. {NEVER_GITHUB}",
        head = pr.head_ref_oid,
        pr_url = pr.url,
        job = excerpt(job),
        tail = tail_of(tail.trim_end()),
    )
}

fn tail_of(text: &str) -> &str {
    let mut start = text.len().saturating_sub(LOG_TAIL_BYTES_MAX);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    assert!(text.len() - start <= LOG_TAIL_BYTES_MAX);
    &text[start..]
}

fn changes(pr: &WirePr, inline: &[WireInline]) -> Result<Vec<Change>, WatchError> {
    for (len, what) in [
        (pr.status_check_rollup.len(), "checks"),
        (pr.reviews.len(), "reviews"),
        (pr.comments.len(), "comments"),
        (inline.len(), "review comments"),
    ] {
        if len > ITEMS_MAX {
            return Err(WatchError::TooMany {
                what,
                max: ITEMS_MAX,
            });
        }
    }
    let (number, head) = (pr.number, pr.head_ref_oid.as_str());
    let mut out = Vec::new();
    out.extend(checks_change(pr));
    for review in pr.reviews.iter().filter(|review| !review.body.is_empty()) {
        let body = excerpt(&review.body);
        let record = json!({"kind": "review", "pr": number, "head": head,
            "state": review.state, "author": review.author.login, "excerpt": body,
            "url": pr.url});
        let asks = review.state == "CHANGES_REQUESTED" && !body.contains('?');
        let fix = asks.then(|| Fix::Change(change_brief(&review.author.login, &pr.url, "", body)));
        out.push(Change {
            key: format!("review:{}", review.id),
            record,
            fix,
        });
    }
    for comment in &pr.comments {
        let record = json!({"kind": "comment", "pr": number, "head": head,
            "author": comment.author.login, "excerpt": excerpt(&comment.body),
            "url": comment.url});
        out.push(Change {
            key: format!("comment:{}", comment.id),
            record,
            fix: None,
        });
    }
    for comment in inline {
        let body = excerpt(&comment.body);
        let record = json!({"kind": "review", "pr": number, "head": head,
            "author": comment.user.login, "file": comment.path, "line": comment.line,
            "excerpt": body, "url": comment.html_url});
        let place = match (&comment.path, comment.line) {
            (Some(path), Some(line)) => format!(" at {path}:{line}"),
            (Some(path), None) => format!(" in {path}"),
            _ => String::new(),
        };
        let brief = change_brief(&comment.user.login, &comment.html_url, &place, body);
        let fix = (!body.contains('?')).then_some(Fix::Change(brief));
        out.push(Change {
            key: format!("inline:{}", comment.id),
            record,
            fix,
        });
    }
    assert!(out.len() <= 1 + 3 * ITEMS_MAX);
    Ok(out)
}

fn checks_change(pr: &WirePr) -> Option<Change> {
    let (number, head) = (pr.number, pr.head_ref_oid.as_str());
    let checks = checks_state(&pr.status_check_rollup);
    if checks == Checks::Pending {
        return None;
    }
    let failed = pr
        .status_check_rollup
        .iter()
        .find(|check| is_failure(check));
    let state = if checks == Checks::Passed {
        "passed"
    } else {
        "failed"
    };
    let mut record = json!({"kind": "checks", "pr": number, "head": head, "state": state,
        "url": pr.url});
    if let Some(check) = failed {
        record["job"] = json!(excerpt(&check.name));
        record["url"] = json!(check.details_url);
    }
    let fix = (checks == Checks::Failed).then_some(Fix::Ci);
    Some(Change {
        key: format!("checks:{number}:{head}"),
        record,
        fix,
    })
}

fn change_brief(author: &str, url: &str, place: &str, body: &str) -> String {
    assert!(!url.is_empty());
    let quoted = body
        .lines()
        .map(|line| format!("> {line}\n"))
        .collect::<String>();
    format!(
        "{author} asked for a change on the pull request{place}, {url}:\n\n{quoted}\nMake \
         the change and commit, or say in this task's thread why not. {NEVER_GITHUB}"
    )
}

fn is_failure(check: &WireCheck) -> bool {
    matches!(
        check.conclusion.as_str(),
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
    )
}

fn checks_state(checks: &[WireCheck]) -> Checks {
    if checks.is_empty() || checks.iter().any(|check| check.status != "COMPLETED") {
        return Checks::Pending;
    }
    if checks.iter().any(is_failure) {
        Checks::Failed
    } else {
        Checks::Passed
    }
}

fn excerpt(text: &str) -> &str {
    let mut end = text.len().min(EXCERPT_BYTES_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    assert!(end <= EXCERPT_BYTES_MAX);
    &text[..end]
}

fn remember(seen: &mut Seen, key: String) {
    if seen.keys.len() >= SEEN_MAX {
        seen.keys.remove(0);
    }
    seen.keys.push(key);
    assert!(seen.keys.len() <= SEEN_MAX);
}

fn parse<T: serde::de::DeserializeOwned>(text: &str, what: &'static str) -> Result<T, WatchError> {
    serde_json::from_str(text).map_err(|error| WatchError::Parse {
        what,
        detail: error.to_string(),
    })
}

fn gh(repo: &Path, args: &[&str]) -> Result<String, WatchError> {
    assert!(!args.is_empty() && repo.is_absolute());
    let line = args.join(" ");
    let mut process = Process::new("gh");
    process.current_dir(repo).args(args);
    for name in ENV_REMOVED {
        process.env_remove(name);
    }
    process
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let limit = Duration::from_secs(GH_SECONDS_MAX);
    let output =
        git::spawn_and_collect(process, &line, limit, None).map_err(|error| WatchError::Gh {
            args: line.clone(),
            detail: error.to_string(),
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if RATE_LIMITED.iter().any(|needle| stderr.contains(needle)) {
        return Err(WatchError::RateLimited);
    }
    if output.code != Some(0) {
        let detail = excerpt(stderr.trim()).to_string();
        return Err(WatchError::Gh { args: line, detail });
    }
    String::from_utf8(output.stdout).map_err(|_| WatchError::Parse {
        what: "gh output",
        detail: "not UTF-8".to_string(),
    })
}

fn read_seen(path: &Path) -> Result<Seen, WatchError> {
    let text = match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Seen::default()),
        Err(error) => return Err(WatchError::State(error.to_string())),
        Ok(meta) if meta.len() > STATE_BYTES_MAX => {
            return Err(WatchError::TooMany {
                what: "state bytes",
                max: STATE_BYTES_MAX as usize,
            });
        }
        Ok(_) => fs::read_to_string(path).map_err(|error| WatchError::State(error.to_string()))?,
    };
    let seen: Seen = parse(&text, "github state")?;
    if seen.keys.len() > SEEN_MAX {
        return Err(WatchError::TooMany {
            what: "seen keys",
            max: SEEN_MAX,
        });
    }
    Ok(seen)
}

fn write_seen(path: &Path, seen: &Seen) -> Result<(), WatchError> {
    assert!(seen.keys.len() <= SEEN_MAX);
    let text = serde_json::to_string(seen).map_err(|error| WatchError::State(error.to_string()))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text).map_err(|error| WatchError::State(error.to_string()))?;
    fs::rename(&temporary, path).map_err(|error| WatchError::State(error.to_string()))
}
