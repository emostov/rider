//! One attempt at one claimed task: check it, run its agent, and settle it on the board.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

type Launch<'a> = dyn Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError> + 'a;
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::board::{
    Amendment, Board, BoardError, MESSAGE_CHARS_MAX, REPLIES_MAX, TaskId, TaskState, TaskView,
    Thread, message_tail, one_line,
};
use crate::brief::{Brief, BriefError, echo, label, parse_brief, parse_id_label};
use crate::cli::{IMPLEMENT_PROMPT, PROMPTS, SLOP_RULES};
use crate::core::TASKS_MAX;
use crate::daemon::SUBTREE;
use crate::feed::Cursor;
use crate::git::{self, GitError};
use crate::harness::{
    self, ATTEMPT_SECONDS_MAX, CommandLine, Exit, HarnessError, Io, LOG_BYTES_MAX,
    PROMPT_BYTES_MAX, Resume, SESSION_BYTES_MAX, SessionId, command_for, stderr_log,
};
use crate::roles::{Harness, RoleSpec, role_in_repo};
use crate::settle::{Outcome, Verdict, judge};
use crate::worktree::{self, BRANCH_LABEL, BoardIdentity, Checkout, WorktreeError};

pub const DELIVERY_ROUNDS_MAX: u8 = 3;
/// Upper bound on the failed attempts after which a task is blocked instead of run again.
pub const ATTEMPTS_MAX: u8 = 9;
/// The failed attempts after which a task is blocked, unless the configuration sets them.
pub const ATTEMPTS_DEFAULT: u8 = 3;
const _: () = assert!(ATTEMPTS_DEFAULT > 0 && ATTEMPTS_DEFAULT <= ATTEMPTS_MAX);
/// Upper bound on the bytes read from the end of a failed attempt's log.
pub const LOG_TAIL_BYTES_MAX: u64 = 16 * 1024;
/// Upper bound on the stacked tasks checked when a branch moves, as `rider-result@2` allows.
pub const STACKED_CHECKS_MAX: usize = 64;
/// Upper bound on the tasks of one branch whose stacked tasks are looked for when it moves.
pub const BRANCH_TASKS_MAX: usize = 32;
/// The lines of a failed attempt's log replied to its task.
pub const LOG_TAIL_LINES: usize = 40;
/// Upper bound on a role's prompt file.
pub const PROMPT_FILE_BYTES_MAX: u64 = 64 * 1024;
/// Upper bound on the logs kept for one attempt that was blocked and unblocked again.
pub const LOG_RUNS_MAX: u16 = 16;
/// The most characters of a failed attempt's log reply, kept well under the board's limit.
pub const TAIL_REPLY_CHARS_MAX: usize = MESSAGE_CHARS_MAX - 1_000;
/// Upper bound on the closed `onto` tasks read to find the unfinished tasks on a branch.
pub const ONTO_READS_MAX: usize = 32;
// JSON writes a character as at most six, so a record with this summary fits one message.
/// How many times start-up reads the board's identity before giving up.
pub const IDENTITY_READS_MAX: u32 = 5;
const IDENTITY_PAUSE_MILLIS: u64 = 1_000;
const SUMMARY_CHARS_MAX: usize = TAIL_REPLY_CHARS_MAX / 6;
const ATTEMPT_LABEL: &str = "attempt";
// The commit the task's first attempt started from, so work an earlier attempt committed counts.
const START_LABEL: &str = "start";
const ATTEMPT_DIGITS_MAX: usize = 3;
const RESULT_SCHEMA: &str = "rider-result@2";
const OF_CHAIN_MAX: usize = 16;
const NO_OUTPUT: &str = "(no output)";
const LOGS_DIR: &str = "logs";
// Beside a task's logs: the Claude session id of its latest completed attempt on one line,
// then the repository it ran in.
const SESSION_FILE: &str = "session";
const SESSION_FILE_BYTES_MAX: usize = 4096;
const RESUMABLE_ROLES: [&str; 2] = ["implement", "light"];
const DELIVERY_NOTE: &str = "These replies were posted in your task's thread while you worked. \
                             They amend the brief, and the latest wins.";
const COMMIT_ASK: &str = "Act on them, commit, and end with a new final message.";
const REPORT_ASK: &str = "Act on them without committing, and end with a new final report.";
const TOO_LONG_NOTE: &str = "Replies posted while the agent worked are too long to deliver to it.";
const THREAD_READS_MAX: u32 = 3;
const THREAD_PAUSE_MILLIS: u64 = 500;
const RETRY_NOTE: &str = "This is a retry: the worktree and branch are reused exactly as the \
                          last attempt left them, uncommitted changes included.";

static BOARD_IDENTITY: Mutex<Option<(String, BoardIdentity)>> = Mutex::new(None);

/// Where rider keeps its files, and how its agents reach the board.
pub struct Env {
    pub home: PathBuf,
    pub profile: String,
    pub board_url: String,
    /// Failed attempts after which a task is blocked instead of run again.
    pub attempts: u8,
}

/// A failure of rider itself that left the task unsettled.
#[derive(Debug)]
pub enum RunError {
    Board(BoardError),
    /// The task is no longer claimed, so it is not rider's to settle.
    LostClaim(TaskState),
}

/// What running a claimed task came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Ran {
    Settled(Verdict),
    WaitingFor(TaskId),
    /// The task was relabeled after its claim: no agent launched and it is still claimed.
    RoleChanged {
        claimed: &'static str,
        found: &'static str,
    },
}

impl From<BoardError> for RunError {
    fn from(error: BoardError) -> RunError {
        RunError::Board(error)
    }
}

struct Block {
    reason: String,
    kind: &'static str,
}

impl Block {
    fn new(kind: &'static str, reason: String) -> Block {
        assert!(!kind.is_empty());
        assert!(
            !reason.trim().is_empty(),
            "a block always states its reason"
        );
        Block { reason, kind }
    }

    fn prepared(self) -> Prepared {
        assert!(!self.kind.is_empty());
        Prepared::Blocked {
            kind: self.kind,
            reason: self.reason,
        }
    }
}

struct Work {
    head: String,
    commits: u64,
}

#[derive(Serialize)]
struct ResultRecord<'a> {
    behind: Vec<u64>,
    branch: Option<&'a str>,
    commits: u64,
    head: &'a str,
    summary: &'a str,
}

struct Evidence {
    clean: bool,
    on_branch: bool,
    head_moved: bool,
    keeps_start: bool,
    work: Work,
}

/// Runs one attempt at one already-claimed task and settles it on the board; a task claimed
/// as `claimed` launches only as that role.
pub fn run_task(
    id: TaskId,
    env: &Env,
    board: &dyn Board,
    claimed: Option<&'static RoleSpec>,
    launch: &Launch,
) -> Result<Ran, RunError> {
    let attempt = match prepare_attempt(id, env, board)? {
        Prepared::Ready(attempt) => attempt,
        Prepared::Blocked { kind, reason } => {
            board.block(id, &reason, kind)?;
            return Ok(Ran::Settled(Verdict::LeaveBlocked));
        }
        Prepared::WaitingFor(of) => {
            board.release(id)?;
            return Ok(Ran::WaitingFor(of));
        }
    };
    let found = attempt.role().label;
    if let Some(claimed) = claimed.filter(|claimed| claimed.label != found) {
        return Ok(Ran::RoleChanged {
            claimed: claimed.label,
            found,
        });
    }
    let launched = attempt.launch(board, launch);
    let judgement = judge_attempt(id, board, &attempt, launched)?;
    settle_attempt(id, env, board, &attempt, &judgement)?;
    Ok(Ran::Settled(judgement.verdict))
}

/// An attempt whose brief was checked and whose worktree is ready for its agent.
pub struct Attempt {
    brief: Brief,
    failed: u16,
    retry: bool,
    checkout: Checkout,
    // Where the task's branch started: its `start` label, else where this attempt started.
    reference: String,
    cmd: CommandLine,
    prompt: String,
    home: PathBuf,
    log: PathBuf,
    delivery: Mutex<Delivery>,
    // Why the task resumes, or starts fresh though `resume=1` asked otherwise.
    resume_note: Option<String>,
    // When `cmd` resumes a session: that session, and the command that starts fresh instead.
    fallback: Option<Fallback>,
    // The log of the fresh start, set once `launch` has given up the resumed session.
    fresh_log: OnceLock<PathBuf>,
    // Set by `stop`: no agent of this attempt starts from then on.
    stopped: AtomicBool,
}

#[derive(Default)]
struct Delivery {
    seen: Option<Cursor>,
    delivered: Vec<Amendment>,
    log: Option<PathBuf>,
}

struct Fallback {
    session: SessionId,
    cmd: CommandLine,
}

impl Attempt {
    /// The role the attempt's agent launches as.
    pub fn role(&self) -> &'static RoleSpec {
        self.brief.role
    }

    /// The branch the attempt works on, when the task has one.
    pub fn branch(&self) -> Option<&str> {
        let branch = self.checkout.branch.as_deref();
        assert!(branch.is_none_or(|branch| !branch.is_empty()));
        branch
    }

    pub fn launch(&self, board: &dyn Board, launch: &Launch) -> Result<Exit, HarnessError> {
        assert!(self.checkout.dir.is_absolute() && self.log.is_absolute());
        assert!(!self.cmd.program.is_empty());
        let id = self.brief.id;
        let Some(start) = self.read_thread(board) else {
            return self.launch_first(launch);
        };
        let epoch = start.epoch;
        self.delivery().seen = Some(start.cursor);
        let mut launched = self.launch_first(launch);
        for round in 1..=DELIVERY_ROUNDS_MAX {
            let held = |thread: &Thread| {
                thread.state == TaskState::Claimed
                    && thread.epoch == epoch
                    && !self.stopped.load(Ordering::SeqCst)
            };
            if !matches!(launched, Ok(Exit::Code(0))) || self.stopped.load(Ordering::SeqCst) {
                break;
            }
            let Some(thread) = self.read_thread(board) else {
                break;
            };
            let new = self.undelivered(&thread);
            if !held(&thread) || new.is_empty() {
                break;
            }
            let Some((cmd, how)) = self.delivery_command(&new) else {
                let _ = board.reply(id, TOO_LONG_NOTE);
                break;
            };
            let note = format!(
                "Delivering {} replies posted while the agent worked: {how} (round {round} of \
                 {DELIVERY_ROUNDS_MAX}).",
                new.len()
            );
            let _ = board.reply(id, &note);
            if !self.read_thread(board).is_some_and(|gate| held(&gate)) {
                break;
            }
            let log = log_path(&self.home, id, self.failed + 1);
            let mut delivery = self.delivery();
            delivery.seen = Some(thread.cursor);
            delivery.delivered.extend(new);
            assert!(delivery.delivered.len() <= REPLIES_MAX * usize::from(DELIVERY_ROUNDS_MAX));
            delivery.log = Some(log.clone());
            drop(delivery);
            launched = launch(&cmd, &self.checkout.dir, &log);
        }
        launched
    }

    fn delivery(&self) -> MutexGuard<'_, Delivery> {
        self.delivery.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn undelivered(&self, thread: &Thread) -> Vec<Amendment> {
        let Some(seen) = self.delivery().seen else {
            return Vec::new();
        };
        let new: Vec<Amendment> = (thread.amendments.iter())
            .filter(|amendment| amendment.id > seen)
            .cloned()
            .collect();
        assert!(new.len() <= thread.amendments.len());
        new
    }

    fn read_thread(&self, board: &dyn Board) -> Option<Thread> {
        let id = self.brief.id;
        for read in 1..=THREAD_READS_MAX {
            match board.thread(id) {
                Ok(thread) => return Some(thread),
                Err(_) if read < THREAD_READS_MAX => {
                    thread::sleep(Duration::from_millis(THREAD_PAUSE_MILLIS));
                }
                Err(error) => {
                    let note = format!(
                        "Rider could not read the task's thread in {THREAD_READS_MAX} tries, so \
                         replies posted while the agent worked may not reach it: {error}."
                    );
                    let _ = board.reply(id, &one_line(&note));
                }
            }
        }
        None
    }

    fn delivery_command(&self, new: &[Amendment]) -> Option<(CommandLine, &'static str)> {
        assert!(!new.is_empty() && new.len() <= REPLIES_MAX);
        let ask = if self.brief.role.commits_expected {
            COMMIT_ASK
        } else {
            REPORT_ASK
        };
        let render = |amendments: &[Amendment]| {
            let mut text = format!("{DELIVERY_NOTE} {ask}");
            for amendment in amendments {
                let by = amendment.profile.as_deref().unwrap_or("the owner");
                text.push_str(&format!("\n\nFrom {by}:\n{}", amendment.text));
            }
            text
        };
        let session = match self.brief.role.harness {
            Harness::Claude => harness::session_id(&self.log()),
            Harness::Codex | Harness::CodexReview => None,
        };
        let dir = &self.checkout.dir;
        let (cmd, how) = match &session {
            Some(session) => {
                let resume = Some(Resume::InPlace(session));
                let cmd = command_for(self.brief.role, &render(new), dir, resume);
                (cmd, "resuming the agent's session")
            }
            None => {
                let mut all = self.delivery().delivered.clone();
                all.extend_from_slice(new);
                let prompt = format!("{}\n\n{RETRY_NOTE}\n\n{}", self.prompt, render(&all));
                let cmd = command_for(self.brief.role, &prompt, dir, None);
                (cmd, "running the agent again in its worktree")
            }
        };
        let mut cmd = cmd.ok()?;
        cmd.env.clone_from(&self.cmd.env);
        let prompt = match &cmd.io {
            Io::Stream => cmd.args.last(),
            Io::PromptOnStdin(prompt) => Some(prompt),
        };
        assert!(prompt.is_some_and(|prompt| prompt.contains(DELIVERY_NOTE)));
        Some((cmd, how))
    }

    fn launch_first(&self, launch: &Launch) -> Result<Exit, HarnessError> {
        assert!(self.fresh_log.get().is_none(), "an attempt launches once");
        let launched = launch(&self.cmd, &self.checkout.dir, &self.log);
        let refused =
            !matches!(launched, Ok(Exit::Code(0))) && harness::ended_before_any_turn(&self.log);
        let stopped = self.stopped.load(Ordering::SeqCst);
        match self.fallback.as_ref().filter(|_| refused && !stopped) {
            Some(fresh) => {
                let log = log_path(&self.home, self.brief.id, self.failed + 1);
                let log = self.fresh_log.get_or_init(|| log);
                launch(&fresh.cmd, &self.checkout.dir, log)
            }
            None => launched,
        }
    }

    /// Keeps `launch` from starting any further agent, such as a refused resume's fresh start.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        assert!(self.stopped.load(Ordering::SeqCst));
    }

    // The log of the agent run whose work the attempt is judged by.
    fn log(&self) -> PathBuf {
        let log = self.delivery().log.clone();
        log.unwrap_or_else(|| self.fresh_log.get().unwrap_or(&self.log).clone())
    }
}

/// What preparing an attempt came to: an attempt to run, or a block to write instead.
pub enum Prepared {
    Ready(Box<Attempt>),
    WaitingFor(TaskId),
    Blocked { kind: &'static str, reason: String },
}

/// How an attempt's agent did, judged from its exit, its log and its worktree.
pub struct Judgement {
    pub verdict: Verdict,
    work: Option<Work>,
}

impl Judgement {
    /// A failure decided without inspecting the attempt's worktree, such as its time limit.
    pub fn uninspected(verdict: Verdict) -> Judgement {
        assert!(matches!(verdict, Verdict::Fail { .. }));
        let judgement = Judgement {
            verdict,
            work: None,
        };
        assert!(judgement.work.is_none());
        judgement
    }
}

/// Checks a claimed task's brief and readies its worktree, labelling its branch and start.
pub fn prepare_attempt(id: TaskId, env: &Env, board: &dyn Board) -> Result<Prepared, RunError> {
    assert!(env.home.is_absolute() && env.home.to_str().is_some());
    assert!(!env.profile.is_empty() && !env.board_url.is_empty());
    let view = board.read_task(id)?;
    if view.state != TaskState::Claimed {
        return Err(RunError::LostClaim(view.state));
    }
    let start_label = label(&view, START_LABEL);
    // A label that is malformed, or names no commit here, is ignored: never fatal.
    let start = start_label.filter(|value| git::is_commit_id(value));
    let (brief, failed) = match validate(&view, env.attempts) {
        Ok(validated) => validated,
        Err(block) => return Ok(block.prepared()),
    };
    if let Some(of) = brief.of.filter(|of| !view.needs.contains(of))
        && let Some(prepared) = of_not_done(id, of, board)?
    {
        return Ok(prepared);
    }
    if let Some(error) = unneeded_on_branch(&brief, &view, board)? {
        return Ok(Block::new("invalid_brief", error.to_string()).prepared());
    }
    let identity = board_identity(&env.board_url, board)?;
    let attempt = match prepare(brief, failed, start, env, board, &identity) {
        Ok(attempt) => attempt,
        Err(block) => return Ok(block.prepared()),
    };
    if let Some(branch) = &attempt.checkout.branch {
        if let Some(block) = label_branch(id, board, &view, branch)? {
            return Ok(block.prepared());
        }
        if !attempt.retry && start_label.is_none() {
            board.set_label(id, START_LABEL, &attempt.checkout.start_head)?;
        }
    }
    if let Some(note) = &attempt.resume_note {
        // The note only informs; losing it must not stop the attempt.
        let _ = board.reply(id, note);
    }
    Ok(Prepared::Ready(Box::new(attempt)))
}

/// The identity of the board at `url`, read from it once per process and then remembered.
pub fn board_identity(url: &str, board: &dyn Board) -> Result<BoardIdentity, BoardError> {
    assert!(!url.is_empty());
    // Held across the read, so the attempts of one daemon post at most one marker between them.
    let mut known = BOARD_IDENTITY
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some((known_url, identity)) = known.as_ref()
        && known_url == url
    {
        return Ok(identity.clone());
    }
    let identity = BoardIdentity::new(&board.identity()?);
    *known = Some((url.to_string(), identity.clone()));
    Ok(identity)
}

/// Why start-up could not learn the identity of the board at `url`.
#[derive(Debug)]
pub struct IdentityUnknown {
    pub url: String,
    pub error: BoardError,
}

impl fmt::Display for IdentityUnknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not read the identity of the board at {} in {IDENTITY_READS_MAX} tries: {}",
            self.url, self.error
        )
    }
}

/// The board's identity, read up to `IDENTITY_READS_MAX` times with a pause between reads.
pub fn board_identity_at_start(
    url: &str,
    board: &dyn Board,
) -> Result<BoardIdentity, IdentityUnknown> {
    assert!(!url.is_empty());
    let mut reads: u32 = 0;
    loop {
        reads += 1;
        assert!(reads <= IDENTITY_READS_MAX);
        match board_identity(url, board) {
            Ok(identity) => return Ok(identity),
            Err(error) if reads == IDENTITY_READS_MAX => {
                let url = url.to_string();
                return Err(IdentityUnknown { url, error });
            }
            Err(_) => thread::sleep(Duration::from_millis(IDENTITY_PAUSE_MILLIS)),
        }
    }
}

/// Judges an attempt once its agent has ended, from the task's state and the worktree.
pub fn judge_attempt(
    id: TaskId,
    board: &dyn Board,
    attempt: &Attempt,
    launched: Result<Exit, HarnessError>,
) -> Result<Judgement, RunError> {
    if let Some(fresh) = attempt.fallback.as_ref()
        && attempt.fresh_log.get().is_some()
    {
        let note = format!(
            "Claude could not resume session {} before doing any work, so the attempt ran \
             again starting fresh.",
            fresh.session.as_str()
        );
        // The note only informs; losing it must not stop the attempt.
        let _ = board.reply(id, &note);
    }
    let (mut verdict, work) = inspect(id, board, attempt, launched)?;
    if matches!(verdict, Verdict::Complete { .. })
        && let Some(thread) = attempt.read_thread(board)
    {
        let missed = attempt.undelivered(&thread);
        if missed.is_empty() {
            let mut delivery = attempt.delivery();
            delivery.seen = delivery.seen.map(|_| thread.cursor);
        } else {
            let reason = format!(
                "{} replies from the lead or owner reached the thread after the agent's last \
                 run and were not delivered to it, so a retry reads them",
                missed.len()
            );
            verdict = Verdict::Fail { reason };
        }
    }
    assert!(!matches!(verdict, Verdict::Complete { .. }) || work.is_some());
    Ok(Judgement { verdict, work })
}

/// Writes a judged attempt to the board: completes or fails the task, or leaves it blocked.
pub fn settle_attempt(
    id: TaskId,
    env: &Env,
    board: &dyn Board,
    attempt: &Attempt,
    judgement: &Judgement,
) -> Result<(), RunError> {
    assert!(attempt.brief.id == id);
    match (&judgement.verdict, &judgement.work) {
        (Verdict::Complete { result }, Some(work)) => {
            complete(id, env, board, attempt, result, work)?
        }
        (Verdict::Complete { .. }, None) => panic!("a completion always has inspected work"),
        (Verdict::Fail { reason }, _) => fail(id, board, attempt, reason)?,
        (Verdict::LeaveBlocked, _) => {}
    }
    Ok(())
}

// A task's `branch` label is written once and never changed to another branch.
fn label_branch(
    id: TaskId,
    board: &dyn Board,
    view: &TaskView,
    branch: &str,
) -> Result<Option<Block>, RunError> {
    assert!(!branch.is_empty());
    assert!(view.id == id);
    match label(view, BRANCH_LABEL) {
        None => board.set_label(id, BRANCH_LABEL, branch)?,
        Some(own) if own == branch => {}
        Some(own) => {
            let reason = format!(
                "The task's `branch` label names {:?} but its worktree is on branch {branch}; \
                 set the label to the branch that holds the task's work.",
                echo(own)
            );
            return Ok(Some(Block::new("rider_error", reason)));
        }
    }
    Ok(None)
}

/// Runs an agent's command for one attempt within rider's time and log limits.
pub fn launch_agent(
    cmd: &CommandLine,
    dir: &Path,
    log: &Path,
    seconds_max: u64,
) -> Result<Exit, HarnessError> {
    assert!(dir.is_absolute() && log.is_absolute());
    assert!(!cmd.program.is_empty());
    assert!(seconds_max > 0 && seconds_max <= ATTEMPT_SECONDS_MAX);
    harness::run(cmd, dir, log, seconds_max, LOG_BYTES_MAX)
}

fn of_not_done(id: TaskId, of: TaskId, board: &dyn Board) -> Result<Option<Prepared>, RunError> {
    assert!(
        id != of,
        "parse_brief rejects an `of` label naming the task itself"
    );
    let mut next = of;
    for _ in 0..OF_CHAIN_MAX {
        let view = board.read_task(next)?;
        match view.state {
            TaskState::Completed if next == of => return Ok(None),
            TaskState::Canceled if next == of => {
                let reason = BriefError::OfCanceled(of).to_string();
                return Ok(Some(Block::new("invalid_brief", reason).prepared()));
            }
            TaskState::Completed | TaskState::Canceled => break,
            TaskState::Waiting
            | TaskState::Ready
            | TaskState::Claimed
            | TaskState::Blocked
            | TaskState::Failed => {}
        }
        match parse_id_label(&view, "of").ok().flatten() {
            Some(after) if after == id => {
                let reason = BriefError::OfCycle(of).to_string();
                return Ok(Some(Block::new("invalid_brief", reason).prepared()));
            }
            Some(after) if after != next => next = after,
            Some(_) | None => break,
        }
    }
    assert!(next != id);
    Ok(Some(Prepared::WaitingFor(of)))
}

fn validate(view: &TaskView, attempts: u8) -> Result<(Brief, u16), Block> {
    assert!(attempts > 0 && attempts <= ATTEMPTS_MAX);
    let invalid = |reason: String| Block::new("invalid_brief", reason);
    let mut brief = parse_brief(view).map_err(|error| invalid(error.to_string()))?;
    brief.role = role_in_repo(brief.role, &brief.repo);
    let failed = match label(view, ATTEMPT_LABEL) {
        None => 0,
        Some(value) => parse_attempts(value).ok_or_else(|| {
            invalid(format!(
                "Change the `attempt` label from {:?} to the number of failed attempts so far, \
                 such as 1.",
                echo(value)
            ))
        })?,
    };
    if failed >= u16::from(attempts) {
        let reason = format!(
            "The task has failed {failed} attempts; change its brief or role and set the \
             `attempt` label to 0 before reopening it."
        );
        return Err(Block::new("attempts_exhausted", reason));
    }
    assert!(failed < u16::from(attempts));
    assert!(brief.id == view.id);
    Ok((brief, failed))
}

// An open task on the branch an `onto` task works must be one it needs, or one that waits for
// it; any other would race it on the branch. A task works the branch named by its own `branch`
// label or, until it has one, by its `onto` task's, in the repository of its own `repo` label.
fn unneeded_on_branch(
    brief: &Brief,
    view: &TaskView,
    board: &dyn Board,
) -> Result<Option<BriefError>, RunError> {
    assert!(brief.id == view.id);
    let Some(onto) = brief.onto else {
        return Ok(None);
    };
    let listing = board.list_open(SUBTREE)?;
    if listing.left_out > 0 {
        let unchecked = listing.left_out;
        return Ok(Some(BriefError::OntoUnchecked { onto, unchecked }));
    }
    let open: Vec<&TaskView> = listing
        .tasks
        .iter()
        .map(|listed| &listed.view)
        .filter(|task| task.id != view.id && in_repo(task, &brief.repo))
        .collect();
    let mut targets: Vec<TaskId> = vec![onto];
    for task in &open {
        let target = label(task, BRANCH_LABEL)
            .is_none()
            .then(|| parse_id_label(task, "onto").ok().flatten())
            .flatten();
        if let Some(target) = target.filter(|target| {
            !targets.contains(target)
                && listing.tasks.iter().all(|listed| listed.view.id != *target)
        }) {
            targets.push(target);
        }
    }
    if targets.len() > ONTO_READS_MAX {
        let unchecked = targets.len() - ONTO_READS_MAX;
        return Ok(Some(BriefError::OntoUnchecked { onto, unchecked }));
    }
    let mut branches: Vec<(TaskId, Option<String>)> = Vec::with_capacity(targets.len());
    for id in targets {
        let target = board.read_task(id)?;
        branches.push((id, label(&target, BRANCH_LABEL).map(str::to_string)));
    }
    let branch_of = |id: TaskId| branches.iter().find(|(read, _)| *read == id);
    let Some((_, Some(branch))) = branch_of(onto) else {
        return Ok(None);
    };
    let on_branch = closure(&open, onto, |task, members| {
        let works_onto = parse_id_label(task, "onto").ok().flatten();
        let own = label(task, BRANCH_LABEL);
        let inherited = works_onto
            .and_then(branch_of)
            .and_then(|(_, b)| b.as_deref());
        own.or(inherited) == Some(branch.as_str())
            || own.is_none() && works_onto.is_some_and(|id| members.contains(&id))
    });
    let after = closure(&open, view.id, |task, members| {
        task.needs.iter().any(|need| members.contains(need))
    });
    let unneeded = open
        .iter()
        .map(|task| task.id)
        .find(|id| on_branch.contains(id) && !after.contains(id) && !view.needs.contains(id));
    assert!(unneeded != Some(view.id));
    Ok(unneeded.map(|unfinished| BriefError::OntoNotNeeded { unfinished, onto }))
}

fn in_repo(task: &TaskView, repo: &Path) -> bool {
    assert!(repo.is_absolute());
    label(task, "repo").is_some_and(|value| Path::new(value) == repo)
}

// `seed` and every task in `open` that `joins` the members found so far, until none joins.
fn closure(
    open: &[&TaskView],
    seed: TaskId,
    joins: impl Fn(&TaskView, &[TaskId]) -> bool,
) -> Vec<TaskId> {
    assert!(open.len() <= TASKS_MAX);
    let mut members = vec![seed];
    for _ in 0..=open.len() {
        let joining: Vec<TaskId> = open
            .iter()
            .filter(|task| !members.contains(&task.id) && joins(task, &members))
            .map(|task| task.id)
            .collect();
        if joining.is_empty() {
            break;
        }
        members.extend(joining);
    }
    assert!(members.len() <= open.len() + 1);
    members
}

fn parse_attempts(value: &str) -> Option<u16> {
    assert!(!value.is_empty(), "empty labels are absent");
    let digits = value.len() <= ATTEMPT_DIGITS_MAX && value.bytes().all(|b| b.is_ascii_digit());
    let failed = value.parse::<u16>().ok().filter(|_| digits);
    assert!(failed.is_none_or(|n| n < 1000));
    failed
}

fn prepare(
    brief: Brief,
    failed: u16,
    start: Option<&str>,
    env: &Env,
    board: &dyn Board,
    identity: &BoardIdentity,
) -> Result<Attempt, Block> {
    assert!(failed < u16::from(ATTEMPTS_MAX));
    let dir = worktree::dir_for(&brief, &env.home);
    // `prepare` reuses anything at the path as found, so its presence marks a retry.
    let retry = fs::symlink_metadata(&dir).is_ok();
    let prompt = build_prompt(&brief, retry)?;
    let session = brief.resume.then(|| resume_session(&brief, &env.home));
    let resume_note = session.as_ref().map(|session| match (session, brief.onto) {
        (Ok(_), Some(onto)) => format!("Resuming the Claude session of task #{}.", onto.0),
        (Ok(_), None) => panic!("a session is only resumed from an `onto` task"),
        (Err(why), _) => format!("Starting a fresh session despite `resume=1`: {why}."),
    });
    let resume = session.and_then(Result::ok);
    let id = brief.id.0.to_string();
    let command = |resume: Option<&SessionId>| {
        let cmd = command_for(brief.role, &prompt, &dir, resume.map(Resume::Fork));
        let mut cmd = cmd.map_err(|error| {
            let reason = format!(
                "Rider cannot build the agent's command: {}.",
                harness_reason(&error)
            );
            Block::new("rider_error", reason)
        })?;
        cmd.env.extend([
            ("BOARD_TASK".to_string(), id.clone()),
            ("BOARD_PROFILE".to_string(), env.profile.clone()),
            ("BOARD_URL".to_string(), env.board_url.clone()),
        ]);
        Ok::<CommandLine, Block>(cmd)
    };
    let cmd = command(resume.as_ref())?;
    let fallback = match resume {
        Some(session) => Some(Fallback {
            session,
            cmd: command(None)?,
        }),
        None => None,
    };
    let checkout = worktree::prepare(&brief, board, &env.home, identity);
    let checkout = checkout.map_err(|error| worktree_block(&error))?;
    assert!(checkout.dir == dir);
    let log = log_path(&env.home, brief.id, failed + 1);
    assert!(log.is_absolute());
    let start = start.filter(|value| git::commit_exists(&checkout.dir, value).unwrap_or(false));
    let reference = start.unwrap_or(&checkout.start_head).to_string();
    assert!(git::is_commit_id(&reference));
    Ok(Attempt {
        brief,
        failed,
        retry,
        reference,
        checkout,
        cmd,
        prompt,
        home: env.home.clone(),
        log,
        delivery: Mutex::new(Delivery::default()),
        resume_note,
        fallback,
        fresh_log: OnceLock::new(),
        stopped: AtomicBool::new(false),
    })
}

fn resume_session(brief: &Brief, home: &Path) -> Result<SessionId, String> {
    assert!(brief.resume && home.is_absolute());
    if !RESUMABLE_ROLES.contains(&brief.role.label) {
        return Err(format!(
            "role {} is not implement or light",
            brief.role.label
        ));
    }
    if brief.role.harness != Harness::Claude {
        return Err(format!(
            "role {} runs {:?}, and only Claude resumes a session",
            brief.role.label, brief.role.harness
        ));
    }
    let onto = brief.onto.ok_or("the task has no `onto` label")?;
    let path = session_path(home, onto);
    let mut text = String::new();
    let read = File::open(&path).and_then(|file| {
        file.take(SESSION_FILE_BYTES_MAX as u64 + 1)
            .read_to_string(&mut text)
    });
    match read {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "task #{} has no completed Claude attempt with a recorded session",
                onto.0
            ));
        }
        Err(error) => return Err(format!("rider cannot read {}: {error}", path.display())),
    }
    let malformed = || {
        format!(
            "task #{}'s recorded session in {} is malformed",
            onto.0,
            path.display()
        )
    };
    let (session, repo) = text.split_once('\n').ok_or_else(malformed)?;
    let repo = repo.strip_suffix('\n').filter(|repo| !repo.is_empty());
    let repo = Path::new(repo.ok_or_else(malformed)?);
    let session = SessionId::parse(session).ok_or_else(malformed)?;
    if repo != brief.repo {
        return Err(format!(
            "task #{}'s session was recorded in repository {}, not this task's {}",
            onto.0,
            repo.display(),
            brief.repo.display()
        ));
    }
    assert!(session.as_str().len() <= SESSION_BYTES_MAX);
    Ok(session)
}

fn session_path(home: &Path, id: TaskId) -> PathBuf {
    assert!(home.is_absolute());
    let path = home
        .join(LOGS_DIR)
        .join(id.0.to_string())
        .join(SESSION_FILE);
    assert!(path.starts_with(home));
    path
}

fn record_session(attempt: &Attempt, home: &Path) -> Result<(), String> {
    let path = session_path(home, attempt.brief.id);
    let session = match attempt.brief.role.harness {
        Harness::Claude => harness::session_id(&attempt.log()),
        Harness::Codex | Harness::CodexReview => None,
    };
    // A repository the file cannot hold is not recorded, so nothing resumes into it.
    let fits = |repo: &&str| repo.len() < SESSION_FILE_BYTES_MAX - SESSION_BYTES_MAX - 2;
    let repo = attempt
        .brief
        .repo
        .to_str()
        .filter(|repo| !repo.contains('\n'))
        .filter(fits);
    let written = match (&session, repo) {
        (Some(session), Some(repo)) => fs::write(&path, format!("{}\n{repo}\n", session.as_str())),
        (None, _) | (_, None) => match fs::remove_file(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    written.map_err(|error| format!("{}: {error}", path.display()))
}

fn log_path(home: &Path, id: TaskId, attempt: u16) -> PathBuf {
    assert!(home.is_absolute());
    assert!(attempt >= 1 && attempt <= u16::from(ATTEMPTS_MAX));
    let dir = home.join(LOGS_DIR).join(id.0.to_string());
    let mut path = dir.join(format!("{attempt}.jsonl"));
    for run in 2..=LOG_RUNS_MAX {
        if fs::symlink_metadata(&path).is_err() {
            break;
        }
        path = dir.join(format!("{attempt}-{run}.jsonl"));
    }
    assert!(path.starts_with(&dir));
    path
}

fn role_prompt(prompt_file: &str) -> Result<String, Block> {
    let built_in = PROMPTS.iter().find(|(name, _)| *name == prompt_file);
    match built_in.map(|(_, text)| text.trim_end()) {
        Some(text) if prompt_file == IMPLEMENT_PROMPT => {
            Ok(format!("{text}\n\n{}", SLOP_RULES.trim_end()))
        }
        Some(text) => Ok(text.to_string()),
        None => read_prompt_file(Path::new(prompt_file)),
    }
}

fn read_prompt_file(path: &Path) -> Result<String, Block> {
    assert!(
        path.is_absolute(),
        "a prompt not built in is an absolute path"
    );
    let unreadable = |error: io::Error| {
        let reason = format!(
            "Rider cannot read its prompt file {}: {error}.",
            path.display()
        );
        Block::new("rider_error", reason)
    };
    let file = File::open(path).map_err(unreadable)?;
    let mut role_prompt = String::new();
    let mut bounded = file.take(PROMPT_FILE_BYTES_MAX + 1);
    bounded
        .read_to_string(&mut role_prompt)
        .map_err(unreadable)?;
    if u64::try_from(role_prompt.len()).map_or(true, |len| len > PROMPT_FILE_BYTES_MAX) {
        let reason = format!(
            "Rider's prompt file {} is larger than {PROMPT_FILE_BYTES_MAX} bytes.",
            path.display()
        );
        return Err(Block::new("rider_error", reason));
    }
    role_prompt.truncate(role_prompt.trim_end().len());
    assert!(!role_prompt.ends_with(char::is_whitespace));
    Ok(role_prompt)
}

fn build_prompt(brief: &Brief, retry: bool) -> Result<String, Block> {
    let role_prompt = role_prompt(brief.role.prompt_file)?;
    let role_prompt = role_prompt.as_str();
    let mut prompt = format!("{role_prompt}\n\n# Task {}\n\n{}", brief.id.0, brief.text);
    if retry {
        prompt.push_str("\n\n");
        prompt.push_str(RETRY_NOTE);
    }
    if !brief.role.skills.is_empty() {
        let names: Vec<String> = brief
            .role
            .skills
            .iter()
            .map(|skill| format!("`{skill}`"))
            .collect();
        prompt.push_str(&format!(
            "\n\nBefore you complete, run each of these skills in order over the task's whole \
             PR range, every commit on its branch, and fix what each one reports: {}.",
            names.join(", then ")
        ));
    }
    assert!(prompt.starts_with(role_prompt));
    assert!(prompt.contains(brief.text.as_str()));
    Ok(prompt)
}

fn harness_reason(error: &HarnessError) -> String {
    match error {
        HarnessError::PromptTooLarge { bytes } => format!(
            "the task text makes the prompt {bytes} bytes, over the limit of \
             {PROMPT_BYTES_MAX}; shorten the task text"
        ),
        HarnessError::PromptUnsafe => {
            "the task text leaves the prompt without a sentence; give the task a sentence"
                .to_string()
        }
        HarnessError::Spawn(_)
        | HarnessError::Log(_)
        | HarnessError::Wait(_)
        | HarnessError::ReadersExhausted => error.to_string(),
    }
}

fn worktree_block(error: &WorktreeError) -> Block {
    let rider = |what: String| {
        Block::new(
            "rider_error",
            format!("Rider cannot prepare the worktree: {what}."),
        )
    };
    let block = match error {
        WorktreeError::BaseHasNoBranch(task) => Block::new(
            "invalid_brief",
            format!(
                "Set the `branch` label on task #{} to its branch, or remove the `base` label \
             from this task.",
                task.0
            ),
        ),
        WorktreeError::OntoHasNoBranch(task) => Block::new(
            "invalid_brief",
            format!(
                "Set the `branch` label on task #{} to its branch, or remove the `onto` label \
             from this task.",
                task.0
            ),
        ),
        WorktreeError::OfHasNoBranch(task) => Block::new(
            "invalid_brief",
            format!(
                "Set the `branch` label on task #{} to the branch to check, or change the `of` \
             label to a task that has one.",
                task.0
            ),
        ),
        WorktreeError::NoFromBranch => Block::new(
            "invalid_brief",
            "Set `from=<branch>` on this task to the branch to cut it from: it has no `base` \
             or `onto`, and the repository has no origin/HEAD to default to."
                .to_string(),
        ),
        WorktreeError::OntoBranchBusy(task) => Block::new(
            "branch_busy",
            format!(
                "Wait until the branch of task #{} is not checked out in another worktree, then \
             unblock this task.",
                task.0
            ),
        ),
        WorktreeError::BranchBusy(branch) => Block::new(
            "branch_busy",
            format!(
                "Wait until branch {branch} is not checked out in another worktree, then unblock \
             this task."
            ),
        ),
        WorktreeError::ForeignBoard { .. } => Block::new(
            "rider_error",
            format!(
                "Rider will not reuse the worktree: {error}. Move it aside or remove it, then unblock this task."
            ),
        ),
        WorktreeError::NotACheckout(_)
        | WorktreeError::BoardRecord { .. }
        | WorktreeError::NotOnBranch { .. }
        | WorktreeError::NotReleasable { .. }
        | WorktreeError::Git(_)
        | WorktreeError::Board(_) => rider(error.to_string()),
    };
    assert!(["invalid_brief", "branch_busy", "rider_error"].contains(&block.kind));
    block
}

fn inspect(
    id: TaskId,
    board: &dyn Board,
    attempt: &Attempt,
    launched: Result<Exit, HarnessError>,
) -> Result<(Verdict, Option<Work>), RunError> {
    let fail = |reason: String| (Verdict::Fail { reason }, None);
    let state_after = board.read_task(id)?.state;
    match state_after {
        TaskState::Claimed => {}
        // A worker that blocked its task owns it now, however its process ended.
        TaskState::Blocked => return Ok((Verdict::LeaveBlocked, None)),
        other => return Err(RunError::LostClaim(other)),
    }
    let exit = match launched {
        Ok(exit) => exit,
        Err(error) => return Ok(fail(error.to_string())),
    };
    let evidence = match evidence(attempt) {
        Ok(evidence) => evidence,
        Err(error) => {
            return Ok(fail(format!(
                "rider could not inspect the worktree: {error}"
            )));
        }
    };
    let harness = &attempt.brief.role.harness;
    let last_message = harness::last_message(harness, &attempt.log());
    let outcome = Outcome {
        exit,
        state_after,
        stream_failed: harness::stream_reports_failure(harness, &attempt.log()),
        clean: evidence.clean,
        branch: attempt.checkout.branch.as_deref(),
        on_branch: evidence.on_branch,
        head_moved: evidence.head_moved,
        keeps_start: evidence.keeps_start,
        own_commits: evidence.work.commits,
        commits_expected: attempt.brief.role.commits_expected,
        last_message: last_message.as_deref(),
    };
    let verdict = judge(&outcome);
    assert!(
        verdict != Verdict::LeaveBlocked,
        "a claimed task is never left blocked"
    );
    Ok((verdict, Some(evidence.work)))
}

// For a task on a branch, the work is the branch's own commit, not wherever HEAD points.
fn evidence(attempt: &Attempt) -> Result<Evidence, String> {
    let checkout = &attempt.checkout;
    let dir = checkout.dir.as_path();
    // A branch is measured from where the task began; a detached checkout from this attempt.
    let start = match checkout.branch {
        Some(_) => attempt.reference.as_str(),
        None => checkout.start_head.as_str(),
    };
    let text = |error: GitError| error.to_string();
    let clean = git::is_clean(dir).map_err(text)?;
    let current = git::current_branch(dir).map_err(text)?;
    let (on_branch, head, keeps_start, commits) = match &checkout.branch {
        Some(branch) => {
            let full = format!("refs/heads/{branch}");
            if !git::is_safe_ref(&full) {
                return Err(format!(
                    "the branch {} cannot be passed to git",
                    echo(branch)
                ));
            }
            let head = git::commit_of(dir, &full).map_err(text)?;
            let keeps_start =
                attempt.brief.restack || git::is_ancestor(dir, start, &head).map_err(text)?;
            let commits = git::own_commit_count(dir, branch, &head, start).map_err(text)?;
            (
                current.as_deref() == Some(branch),
                head,
                keeps_start,
                commits,
            )
        }
        None => (current.is_none(), git::head(dir).map_err(text)?, true, 0),
    };
    assert!(!head.is_empty());
    assert!(checkout.branch.is_some() || keeps_start);
    Ok(Evidence {
        clean,
        on_branch,
        head_moved: head != start,
        keeps_start,
        work: Work { head, commits },
    })
}

fn complete(
    id: TaskId,
    env: &Env,
    board: &dyn Board,
    attempt: &Attempt,
    result: &str,
    work: &Work,
) -> Result<(), RunError> {
    assert!(
        !result.trim().is_empty(),
        "a completion always states its result"
    );
    let end = result.char_indices().nth(SUMMARY_CHARS_MAX);
    let summary = &result[..end.map_or(result.len(), |(index, _)| index)];
    assert!(!summary.trim().is_empty() && summary.chars().count() <= SUMMARY_CHARS_MAX);
    // The fenced write goes first: a retry writes nothing twice, and a lost claim nothing.
    board.complete(id, summary)?;
    // `vb tasks complete` has no precondition on the thread, so a reply posted during the
    // write is only found afterwards.
    if let Some(thread) = attempt.read_thread(board) {
        let missed = attempt.undelivered(&thread);
        if !missed.is_empty() {
            let note = format!(
                "{} replies from the lead or owner landed while rider completed the task and \
                 were not delivered to its agent.",
                missed.len()
            );
            let _ = board.reply(id, &note);
        }
    }
    let behind = match (attempt.brief.onto, attempt.branch()) {
        (Some(onto), Some(branch)) if work.head != attempt.reference => {
            let (behind, cut) = stacked_behind(board, attempt, onto, branch, &work.head)
                .unwrap_or_else(|error| (Vec::new(), Some(error)));
            if let Some(cut) = cut {
                let note = format!(
                    "Rider could not check every task stacked on branch {branch}, so its \
                     result may name too few behind: {cut}."
                );
                let _ = board.reply(id, &one_line(&note));
            }
            behind
        }
        _ => Vec::new(),
    };
    let record = ResultRecord {
        behind,
        branch: attempt.checkout.branch.as_deref(),
        commits: work.commits,
        head: &work.head,
        summary,
    };
    if let Err(error) = record_session(attempt, &env.home) {
        let note = format!("Rider could not record the agent's session: {error}.");
        let _ = board.reply(id, &one_line(&note));
    }
    // The record and the note below only inform; losing them must not undo finished work.
    if let Ok(record) = serde_json::to_string(&record)
        && board.post_structured(id, RESULT_SCHEMA, &record).is_err()
    {
        let _ = board.reply(id, &record);
    }
    if let Err(error) = worktree::release(&attempt.checkout, &attempt.brief.repo, &env.home) {
        let why = match error {
            WorktreeError::NotReleasable { reason, .. } => reason.to_string(),
            other => other.to_string(),
        };
        let dir = attempt.checkout.dir.display();
        // The warning comes before git's reason, which may be long enough to be cut short.
        let held = git::current_branch(&attempt.checkout.dir).ok().flatten();
        let note = match held {
            Some(branch) => format!(
                "Kept the worktree at {dir}, so branch {branch} stays checked out there: tasks \
                 working `onto` it will block as branch_busy until a person removes or fixes \
                 the worktree at {dir}. It was kept because {why}."
            ),
            None => format!("Kept the worktree at {dir}: {why}."),
        };
        let _ = board.reply(id, &one_line(&note));
    }
    Ok(())
}

// A branch's tasks are those labelled with it in the completing task's channel and repository,
// and `onto`. Returns that channel, their ids, and why the list was cut short at a bound.
fn branch_tasks(
    board: &dyn Board,
    attempt: &Attempt,
    onto: TaskId,
    branch: &str,
) -> Result<(String, Vec<u64>, Option<String>), String> {
    assert!(attempt.brief.onto == Some(onto));
    let id = attempt.brief.id;
    if branch.contains('|') {
        return Err("its name holds `|`, which a label filter cannot name".to_string());
    }
    let named = board
        .list_labelled(SUBTREE, BRANCH_LABEL, &[branch])
        .map_err(|error| error.to_string())?;
    let mut cut = (named.left_out > 0).then(|| {
        format!(
            "{} of its tasks were past the listing's bound",
            named.left_out
        )
    });
    let own = named.tasks.iter().find(|task| task.view.id == id);
    let channel = own.ok_or_else(|| format!("task #{} is not listed on it", id.0))?;
    let channel = &channel.channel;
    let mut on_branch: Vec<u64> = vec![onto.0];
    let mine = (named.tasks.iter())
        .filter(|task| task.channel == *channel && same_repo(&task.view, &attempt.brief.repo))
        .filter(|task| label(&task.view, BRANCH_LABEL) == Some(branch));
    for task in mine {
        if on_branch.contains(&task.view.id.0) {
            continue;
        }
        if on_branch.len() == BRANCH_TASKS_MAX {
            cut.get_or_insert(format!("it holds more than {BRANCH_TASKS_MAX} tasks"));
            break;
        }
        on_branch.push(task.view.id.0);
    }
    assert!(!on_branch.is_empty() && on_branch.len() <= BRANCH_TASKS_MAX);
    Ok((channel.clone(), on_branch, cut))
}

fn same_repo(view: &TaskView, repo: &Path) -> bool {
    label(view, "repo").is_some_and(|own| Path::new(own) == repo)
}

// A task is stacked on `branch` when its `base` names one of the branch's tasks, and behind
// when its own branch does not contain `head`. Besides the behind tasks, returns why the
// check was cut short when a bound was reached.
fn stacked_behind(
    board: &dyn Board,
    attempt: &Attempt,
    onto: TaskId,
    branch: &str,
    head: &str,
) -> Result<(Vec<u64>, Option<String>), String> {
    assert!(git::is_commit_id(head), "a branch head is a commit id");
    let dir = &attempt.checkout.dir;
    let (channel, on_branch, mut cut) = branch_tasks(board, attempt, onto, branch)?;
    let names: Vec<String> = (on_branch.iter())
        .flat_map(|task| [task.to_string(), format!("#{task}")])
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let stacked = board
        .list_labelled(&channel, "base", &names)
        .map_err(|error| error.to_string())?;
    if stacked.left_out > 0 {
        let left_out = stacked.left_out;
        cut.get_or_insert(format!(
            "{left_out} stacked tasks were past the listing's bound"
        ));
    }
    let id_of = |value: &str| value.strip_prefix('#').unwrap_or(value).parse::<u64>().ok();
    let mut behind: Vec<u64> = Vec::new();
    let mut checked: usize = 0;
    for view in stacked
        .tasks
        .iter()
        .map(|task| &task.view)
        .filter(|view| same_repo(view, &attempt.brief.repo))
    {
        let base = label(view, "base").and_then(id_of);
        let Some(own) = label(view, BRANCH_LABEL).filter(|own| *own != branch) else {
            continue;
        };
        if !base.is_some_and(|base| on_branch.contains(&base)) {
            continue;
        }
        if checked == STACKED_CHECKS_MAX {
            cut.get_or_insert(format!(
                "more than {STACKED_CHECKS_MAX} tasks are stacked on it"
            ));
            break;
        }
        checked += 1;
        let full = format!("refs/heads/{own}");
        let text = |error: GitError| error.to_string();
        // A stacked task whose branch is not here, or cannot be named to git, is not judged.
        let safe = git::is_safe_ref(own) && git::is_safe_ref(&full);
        if !safe || !git::branch_exists(dir, own).map_err(text)? {
            continue;
        }
        let own_head = git::commit_of(dir, &full).map_err(text)?;
        if !git::is_ancestor(dir, head, &own_head).map_err(text)? {
            behind.push(view.id.0);
        }
    }
    assert!(behind.len() <= checked && checked <= STACKED_CHECKS_MAX);
    Ok((behind, cut))
}

fn fail(id: TaskId, board: &dyn Board, attempt: &Attempt, reason: &str) -> Result<(), RunError> {
    assert!(
        !reason.trim().is_empty(),
        "a failure always states its reason"
    );
    assert!(attempt.failed < u16::from(ATTEMPTS_MAX));
    // `codex review` prints errors, such as a provider's content-filter stop, only on stderr.
    let (header, tail) = match attempt.brief.role.harness {
        harness @ (Harness::Claude | Harness::Codex) => {
            let log = attempt.log();
            let tail = log_tail(&log);
            let tail = harness::failure_account(&harness, &log, &tail).unwrap_or(tail);
            (format!("The whole log: {}", log.display()), tail)
        }
        Harness::CodexReview => {
            let stderr = stderr_log(&attempt.log());
            let header = format!(
                "The final text: {}\nThe whole transcript: {}",
                attempt.log().display(),
                stderr.display()
            );
            let tail = format!("{}\n{}", log_tail(&stderr), log_tail(&attempt.log()));
            (header, tail)
        }
    };
    let budget = TAIL_REPLY_CHARS_MAX.saturating_sub(header.chars().count() + 1);
    let tail = if tail.trim().is_empty() {
        NO_OUTPUT
    } else {
        message_tail(&tail, budget)
    };
    // The fenced write goes first, so nothing is written for a task whose claim was lost.
    board.fail(id, reason)?;
    let failed = attempt.failed + 1;
    // The task is failed already: a lost label or log reply must not retry the `fail`.
    if board
        .set_label(id, ATTEMPT_LABEL, &failed.to_string())
        .is_err()
    {
        let note = format!("Rider could not set the `attempt` label to {failed}.");
        let _ = board.reply(id, &note);
    }
    if board.reply(id, &format!("{header}\n{tail}")).is_err() {
        let _ = board.reply(id, &header);
    }
    Ok(())
}

// The window past its first byte, which is the one before the window, read only for its `\n`.
fn long_line(bytes: &[u8]) -> String {
    assert!(u64::try_from(bytes.len()).is_ok_and(|n| n <= LOG_TAIL_BYTES_MAX + 1));
    let window = String::from_utf8_lossy(bytes.get(1..).unwrap_or_default());
    let kib = LOG_TAIL_BYTES_MAX / 1024;
    let tail = format!("(last line longer than {kib} KiB, truncated)\n{window}");
    assert!(tail.ends_with(window.as_ref()));
    tail
}

fn log_tail(log: &Path) -> String {
    let Ok(mut file) = File::open(log) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    // One byte before the window shows whether the window starts on a whole line.
    let from = len.saturating_sub(LOG_TAIL_BYTES_MAX + 1);
    let mut bytes = Vec::new();
    let read = file
        .seek(SeekFrom::Start(from))
        .and_then(|_| file.take(LOG_TAIL_BYTES_MAX + 1).read_to_end(&mut bytes));
    if read.is_err() {
        return String::new();
    }
    let whole = if from == 0 {
        &bytes[..]
    } else {
        match bytes.iter().position(|b| *b == b'\n') {
            Some(newline) => &bytes[newline + 1..],
            None => return long_line(&bytes),
        }
    };
    let text = String::from_utf8_lossy(whole);
    let text = text.trim_end_matches('\n');
    let begin = text.rmatch_indices('\n').nth(LOG_TAIL_LINES - 1);
    let tail = text[begin.map_or(0, |(newline, _)| newline + 1)..].to_string();
    assert!(tail.lines().count() <= LOG_TAIL_LINES);
    assert!(u64::try_from(tail.len()).is_ok_and(|n| n <= 3 * (LOG_TAIL_BYTES_MAX + 1)));
    tail
}
