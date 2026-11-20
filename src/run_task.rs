//! One attempt at one claimed task: check it, run its agent, and settle it on the board.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::board::{
    Board, BoardError, MESSAGE_CHARS_MAX, TaskId, TaskState, TaskView, message_tail, one_line,
};
use crate::brief::{Brief, echo, label, parse_brief};
use crate::git::{self, GitError};
use crate::harness::{
    self, ATTEMPT_SECONDS_MAX, CommandLine, Exit, HarnessError, LOG_BYTES_MAX, PROMPT_BYTES_MAX,
    SESSION_BYTES_MAX, SessionId, command_for,
};
use crate::roles::{Harness, RoleSpec};
use crate::settle::{Outcome, Verdict, judge};
use crate::worktree::{self, BRANCH_LABEL, Checkout, WorktreeError};

/// Upper bound on the failed attempts after which a task is blocked instead of run again.
pub const ATTEMPTS_MAX: u8 = 9;
/// The failed attempts after which a task is blocked, unless the configuration sets them.
pub const ATTEMPTS_DEFAULT: u8 = 3;
const _: () = assert!(ATTEMPTS_DEFAULT > 0 && ATTEMPTS_DEFAULT <= ATTEMPTS_MAX);
/// Upper bound on the bytes read from the end of a failed attempt's log.
pub const LOG_TAIL_BYTES_MAX: u64 = 16 * 1024;
/// The lines of a failed attempt's log replied to its task.
pub const LOG_TAIL_LINES: usize = 40;
/// Upper bound on a role's prompt file.
pub const PROMPT_FILE_BYTES_MAX: u64 = 64 * 1024;
/// Upper bound on the logs kept for one attempt that was blocked and unblocked again.
pub const LOG_RUNS_MAX: u16 = 16;
/// The most characters of a failed attempt's log reply, kept well under the board's limit.
pub const TAIL_REPLY_CHARS_MAX: usize = MESSAGE_CHARS_MAX - 1_000;
// JSON writes a character as at most six, so a record with this summary fits one message.
const SUMMARY_CHARS_MAX: usize = TAIL_REPLY_CHARS_MAX / 6;
const ATTEMPT_LABEL: &str = "attempt";
// The commit the task's first attempt started from, so work an earlier attempt committed counts.
const START_LABEL: &str = "start";
const ATTEMPT_DIGITS_MAX: usize = 3;
const RESULT_SCHEMA: &str = "rider-result@1";
const NO_OUTPUT: &str = "(no output)";
const LOGS_DIR: &str = "logs";
// Beside a task's logs: the Claude session id of its latest completed attempt on one line,
// then the repository it ran in.
const SESSION_FILE: &str = "session";
const SESSION_FILE_BYTES_MAX: usize = 4096;
const RESUMABLE_ROLES: [&str; 2] = ["implement", "light"];
const RETRY_NOTE: &str = "This is a retry: the worktree and branch are reused exactly as the \
                          last attempt left them, uncommitted changes included.";

/// Where rider keeps its files, and how its agents reach the board.
pub struct Env {
    pub home: PathBuf,
    pub prompts: PathBuf,
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
    launch: &dyn Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError>,
) -> Result<Ran, RunError> {
    let attempt = match prepare_attempt(id, env, board)? {
        Prepared::Ready(attempt) => attempt,
        Prepared::Blocked { kind, reason } => {
            board.block(id, &reason, kind)?;
            return Ok(Ran::Settled(Verdict::LeaveBlocked));
        }
    };
    let found = attempt.role().label;
    if let Some(claimed) = claimed.filter(|claimed| claimed.label != found) {
        return Ok(Ran::RoleChanged {
            claimed: claimed.label,
            found,
        });
    }
    let launched = attempt.launch(launch);
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
    log: PathBuf,
    // Why the task resumes, or starts fresh though `resume=1` asked otherwise.
    resume_note: Option<String>,
    // When `cmd` resumes a session: that session, and the command that starts fresh instead.
    fallback: Option<Fallback>,
    // The log of the fresh start, set once `launch` has given up the resumed session.
    fresh_log: OnceLock<PathBuf>,
    // Set by `stop`: no agent of this attempt starts from then on.
    stopped: AtomicBool,
}

struct Fallback {
    session: SessionId,
    cmd: CommandLine,
    home: PathBuf,
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

    /// Runs the attempt's agent in its worktree through `launch`, logging to its own log.
    pub fn launch(
        &self,
        launch: &dyn Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError>,
    ) -> Result<Exit, HarnessError> {
        assert!(self.checkout.dir.is_absolute() && self.log.is_absolute());
        assert!(!self.cmd.program.is_empty());
        assert!(self.fresh_log.get().is_none(), "an attempt launches once");
        let launched = launch(&self.cmd, &self.checkout.dir, &self.log);
        let refused =
            !matches!(launched, Ok(Exit::Code(0))) && harness::ended_before_any_turn(&self.log);
        let stopped = self.stopped.load(Ordering::SeqCst);
        match self.fallback.as_ref().filter(|_| refused && !stopped) {
            Some(fresh) => {
                let log = log_path(&fresh.home, self.brief.id, self.failed + 1);
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
    fn log(&self) -> &Path {
        self.fresh_log.get().unwrap_or(&self.log)
    }
}

/// What preparing an attempt came to: an attempt to run, or a block to write instead.
pub enum Prepared {
    Ready(Box<Attempt>),
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
    let prepared = validate(&view, env.attempts)
        .and_then(|(brief, failed)| prepare(brief, failed, start, env, board));
    let attempt = match prepared {
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
    let (verdict, work) = inspect(id, board, attempt, launched)?;
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
pub fn launch_agent(cmd: &CommandLine, dir: &Path, log: &Path) -> Result<Exit, HarnessError> {
    assert!(dir.is_absolute() && log.is_absolute());
    assert!(!cmd.program.is_empty());
    harness::run(cmd, dir, log, ATTEMPT_SECONDS_MAX, LOG_BYTES_MAX)
}

fn validate(view: &TaskView, attempts: u8) -> Result<(Brief, u16), Block> {
    assert!(attempts > 0 && attempts <= ATTEMPTS_MAX);
    let invalid = |reason: String| Block::new("invalid_brief", reason);
    let brief = parse_brief(view).map_err(|error| invalid(error.to_string()))?;
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
) -> Result<Attempt, Block> {
    assert!(failed < u16::from(ATTEMPTS_MAX));
    let dir = worktree::dir_for(&brief, &env.home);
    // `prepare` reuses anything at the path as found, so its presence marks a retry.
    let retry = fs::symlink_metadata(&dir).is_ok();
    let prompt = build_prompt(env, &brief, retry)?;
    let session = brief.resume.then(|| resume_session(&brief, &env.home));
    let resume_note = session.as_ref().map(|session| match (session, brief.onto) {
        (Ok(_), Some(onto)) => format!("Resuming the Claude session of task #{}.", onto.0),
        (Ok(_), None) => panic!("a session is only resumed from an `onto` task"),
        (Err(why), _) => format!("Starting a fresh session despite `resume=1`: {why}."),
    });
    let resume = session.and_then(Result::ok);
    let id = brief.id.0.to_string();
    let command = |resume: Option<&SessionId>| {
        let mut cmd = command_for(brief.role, &prompt, &dir, resume).map_err(|error| {
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
            home: env.home.clone(),
        }),
        None => None,
    };
    let checkout = worktree::prepare(&brief, board, &env.home);
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
        log,
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
        Harness::Claude => harness::session_id(attempt.log()),
        Harness::Codex | Harness::Tact => None,
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

fn build_prompt(env: &Env, brief: &Brief, retry: bool) -> Result<String, Block> {
    let path = env.prompts.join(brief.role.prompt_file);
    let unreadable = |error: io::Error| {
        let reason = format!(
            "Rider cannot read its prompt file {}: {error}.",
            path.display()
        );
        Block::new("rider_error", reason)
    };
    let file = File::open(&path).map_err(unreadable)?;
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
    let role_prompt = role_prompt.trim_end();
    let mut prompt = format!("{role_prompt}\n\n# Task {}\n\n{}", brief.id.0, brief.text);
    if retry {
        prompt.push_str("\n\n");
        prompt.push_str(RETRY_NOTE);
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
        WorktreeError::NotACheckout(_)
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
    let last_message = harness::last_message(harness, attempt.log());
    let outcome = Outcome {
        exit,
        state_after,
        stream_failed: harness::stream_reports_failure(harness, attempt.log()),
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
            // A restack rebases its branch, so only it may leave its starting commit behind.
            let restacks = attempt.brief.onto.is_some();
            let keeps_start = restacks || git::is_ancestor(dir, start, &head).map_err(text)?;
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
    let record = ResultRecord {
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
    board.complete(id, summary)?;
    Ok(())
}

fn fail(id: TaskId, board: &dyn Board, attempt: &Attempt, reason: &str) -> Result<(), RunError> {
    assert!(
        !reason.trim().is_empty(),
        "a failure always states its reason"
    );
    assert!(attempt.failed < u16::from(ATTEMPTS_MAX));
    let header = format!("The whole log: {}", attempt.log().display());
    let tail = log_tail(attempt.log());
    let budget = TAIL_REPLY_CHARS_MAX.saturating_sub(header.chars().count() + 1);
    let tail = if tail.trim().is_empty() {
        NO_OUTPUT
    } else {
        message_tail(&tail, budget)
    };
    // The tail only informs; losing it must not leave the attempt uncounted.
    if board.reply(id, &format!("{header}\n{tail}")).is_err() {
        let _ = board.reply(id, &header);
    }
    let failed = attempt.failed + 1;
    // Counting before failing means a lost `fail` over-counts, never under-counts.
    board.set_label(id, ATTEMPT_LABEL, &failed.to_string())?;
    board.fail(id, reason)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::TaskView;
    use crate::board::testing::FakeBoard;
    use crate::feed::Cursor;
    use crate::git::testing::{Fixture, git};
    use serde_json::json;
    use std::cell::RefCell;
    use std::fs;

    const ID: TaskId = TaskId(7);
    const BRANCH: &str = "rider/7-extract-token-store";
    const ROLE_PROMPT: &str = "You implement one task.\n";
    const TEXT: &str = "Extract token store\nMove the store into its own module.";
    const DONE_STREAM: &str =
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Extracted the store."}"#;
    const RETRY_NOTE: &str = "This is a retry: the worktree and branch are reused exactly as \
         the last attempt left them, uncommitted changes included.";

    type Launch<'a> = dyn Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError> + 'a;

    fn setup(name: &str) -> (Fixture, Env) {
        let fixture = Fixture::new(name);
        let prompts = fixture.root.join("prompts");
        fs::create_dir_all(&prompts).unwrap();
        fs::write(prompts.join("implement.md"), ROLE_PROMPT).unwrap();
        fs::write(prompts.join("review.md"), "You review one task.\n").unwrap();
        let env = Env {
            home: fixture.home.clone(),
            prompts,
            profile: "implement-1-opus-5-5".to_string(),
            board_url: "http://127.0.0.1:4100".to_string(),
            attempts: ATTEMPTS_DEFAULT,
        };
        (fixture, env)
    }

    fn view(id: u64, labels: &[(&str, &str)], state: TaskState) -> TaskView {
        let labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string()));
        TaskView {
            id: TaskId(id),
            text: TEXT.to_string(),
            state,
            labels: labels.collect(),
            needs: Vec::new(),
            epoch: 1,
            claim: None,
            version: Cursor(0),
        }
    }

    fn task(fixture: &Fixture, extra: &[(&str, &str)]) -> TaskView {
        let repo = fixture.repo.to_str().unwrap();
        let mut labels = vec![("role", "implement"), ("repo", repo), ("from", "main")];
        labels.extend_from_slice(extra);
        view(ID.0, &labels, TaskState::Claimed)
    }

    fn start_call(fixture: &Fixture, rev: &str) -> String {
        format!(
            "set_label 7 start={}",
            git(&fixture.repo, &["rev-parse", rev])
        )
    }

    fn tail_reply(env: &Env, run: u16, tail: &str) -> String {
        format!("reply 7 {}\n{tail}", log_line(env, run))
    }

    fn log_line(env: &Env, run: u16) -> String {
        let log = env.home.join("logs").join("7").join(format!("{run}.jsonl"));
        format!("The whole log: {}", log.display())
    }

    fn calls(board: &FakeBoard) -> Vec<String> {
        board.calls.borrow().clone()
    }

    fn worktree(env: &Env) -> PathBuf {
        env.home.join("worktrees").join("repo").join("7")
    }

    fn never(_: &CommandLine, _: &Path, _: &Path) -> Result<Exit, HarnessError> {
        panic!("no agent may be spawned")
    }

    fn write_log(log: &Path, text: &str) {
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(log, text).unwrap();
    }

    fn committing_agent(
        fixture: &Fixture,
    ) -> impl Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError> + '_ {
        move |_, dir, log| {
            fixture.commit(dir, "store.rs", "pub struct Store;");
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        }
    }

    fn record(branch: Option<&str>, head: &str, commits: u64, summary: &str) -> String {
        let record =
            json!({"branch": branch, "head": head, "commits": commits, "summary": summary});
        format!("post_structured 7 rider-result@1 {record}")
    }

    fn run(env: &Env, board: &FakeBoard, launch: &Launch) -> Verdict {
        match run_task(ID, env, board, None, launch).unwrap() {
            Ran::Settled(verdict) => verdict,
            ran => panic!("expected a settled run, got {ran:?}"),
        }
    }

    fn completed(result: &str) -> Verdict {
        Verdict::Complete {
            result: result.to_string(),
        }
    }

    #[test]
    fn invalid_brief_blocks_and_spawns_nothing() {
        let (_fixture, env) = setup("run-invalid");
        let board = FakeBoard::new(vec![view(7, &[("role", "implement")], TaskState::Claimed)]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let reason = "Set the `repo` label to the absolute path of the repository.";
        assert_eq!(
            calls(&board),
            vec![format!("block 7 invalid_brief {reason}")]
        );
        assert!(!env.home.join("worktrees").exists());
    }

    #[test]
    fn a_malformed_attempt_label_blocks_as_an_invalid_brief() {
        let (fixture, env) = setup("run-attempt-label");
        let board = FakeBoard::new(vec![task(&fixture, &[("attempt", "two")])]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let reason = "Change the `attempt` label from \"two\" to the number of failed \
                      attempts so far, such as 1.";
        assert_eq!(
            calls(&board),
            vec![format!("block 7 invalid_brief {reason}")]
        );
    }

    #[test]
    fn fourth_attempt_is_blocked_as_attempts_exhausted() {
        let (fixture, env) = setup("run-exhausted");
        let board = FakeBoard::new(vec![task(&fixture, &[("attempt", "3")])]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let reason = "The task has failed 3 attempts; change its brief or role and set the \
                      `attempt` label to 0 before reopening it.";
        assert_eq!(
            calls(&board),
            vec![format!("block 7 attempts_exhausted {reason}")]
        );
        assert!(!worktree(&env).exists());
    }

    #[test]
    fn a_base_task_without_a_branch_blocks_as_an_invalid_brief() {
        let (fixture, env) = setup("run-base");
        let base = view(3, &[], TaskState::Completed);
        let board = FakeBoard::new(vec![task(&fixture, &[("base", "#3")]), base]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let reason = "Set the `branch` label on task #3 to its branch, or remove the `base` \
                      label from this task.";
        assert_eq!(
            calls(&board),
            vec![format!("block 7 invalid_brief {reason}")]
        );
    }

    #[test]
    fn a_root_task_without_from_in_a_repository_without_a_remote_is_an_invalid_brief() {
        let (fixture, env) = setup("run-no-from");
        let repo = fixture.repo.to_str().unwrap();
        let root = view(
            7,
            &[("role", "implement"), ("repo", repo)],
            TaskState::Claimed,
        );
        let board = FakeBoard::new(vec![root]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let calls = calls(&board);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("block 7 invalid_brief "), "{calls:?}");
        assert!(calls[0].contains("`from=<branch>`"), "{calls:?}");
        assert!(!worktree(&env).exists());
    }

    #[test]
    fn a_missing_prompt_file_blocks_as_a_rider_error_before_any_worktree() {
        let (fixture, env) = setup("run-prompt");
        let path = env.prompts.join("implement.md");
        fs::remove_file(&path).unwrap();
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let error = fs::File::open(&path).unwrap_err();
        let reason = format!(
            "Rider cannot read its prompt file {}: {error}.",
            path.display()
        );
        assert_eq!(calls(&board), vec![format!("block 7 rider_error {reason}")]);
        assert!(!worktree(&env).exists());
    }

    #[test]
    fn task_text_too_large_for_a_prompt_blocks_before_any_worktree() {
        let (fixture, env) = setup("run-huge");
        let mut huge = task(&fixture, &[]);
        huge.text = format!("Extract token store\n{}", "x".repeat(PROMPT_BYTES_MAX));
        let bytes = format!("You implement one task.\n\n# Task 7\n\n{}", huge.text).len();
        let board = FakeBoard::new(vec![huge]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let reason = format!(
            "Rider cannot build the agent's command: the task text makes the prompt {bytes} \
             bytes, over the limit of {PROMPT_BYTES_MAX}; shorten the task text."
        );
        assert_eq!(calls(&board), vec![format!("block 7 rider_error {reason}")]);
        assert!(!worktree(&env).exists());
    }

    #[test]
    fn successful_implement_task_is_completed_with_its_branch_label() {
        let (fixture, env) = setup("run-ok");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let seen = RefCell::new(None);
        let agent = committing_agent(&fixture);
        let launch = |cmd: &CommandLine, dir: &Path, log: &Path| {
            *seen.borrow_mut() = Some((cmd.clone(), dir.to_path_buf(), log.to_path_buf()));
            agent(cmd, dir, log)
        };
        assert_eq!(
            run(&env, &board, &launch),
            completed("Extracted the store.")
        );
        let head = git(&fixture.repo, &["rev-parse", BRANCH]);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            record(Some(BRANCH), &head, 1, "Extracted the store."),
            "complete 7 Extracted the store.".to_string(),
        ];
        assert_eq!(calls(&board), expected);
        assert!(
            !worktree(&env).exists(),
            "a completed task's worktree is released"
        );
        let (cmd, dir, log) = seen.take().unwrap();
        assert_eq!(dir, worktree(&env));
        assert_eq!(log, env.home.join("logs").join("7").join("1.jsonl"));
        let prompt = format!("You implement one task.\n\n# Task 7\n\n{TEXT}");
        assert_eq!(cmd.args.last(), Some(&prompt));
        let board_env = [
            ("BOARD_TASK", "7"),
            ("BOARD_PROFILE", "implement-1-opus-5-5"),
            ("BOARD_URL", "http://127.0.0.1:4100"),
        ];
        let board_env: Vec<_> = board_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(cmd.env, board_env);
    }

    #[test]
    fn a_completed_task_whose_record_cannot_be_posted_still_completes() {
        let (fixture, env) = setup("run-record");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        board
            .failing
            .borrow_mut()
            .extend(["post_structured", "reply"]);
        let verdict = run(&env, &board, &committing_agent(&fixture));
        assert_eq!(verdict, completed("Extracted the store."));
        let head = git(&fixture.repo, &["rev-parse", BRANCH]);
        let json = json!({"branch": BRANCH, "head": head, "commits": 1,
            "summary": "Extracted the store."});
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            record(Some(BRANCH), &head, 1, "Extracted the store."),
            format!("reply 7 {json}"),
            "complete 7 Extracted the store.".to_string(),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn a_worktree_that_cannot_be_released_is_kept_and_the_task_completes() {
        let (fixture, env) = setup("run-keep");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let launch = |_: &CommandLine, dir: &Path, log: &Path| {
            fixture.commit(dir, "store.rs", "pub struct Store;");
            git(dir, &["bisect", "start"]);
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(
            run(&env, &board, &launch),
            completed("Extracted the store.")
        );
        let head = git(&fixture.repo, &["rev-parse", BRANCH]);
        let dir = worktree(&env);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            record(Some(BRANCH), &head, 1, "Extracted the store."),
            format!(
                "reply 7 Kept the worktree at {dir}, so branch {BRANCH} stays checked out \
                 there: tasks working `onto` it will block as branch_busy until a person \
                 removes or fixes the worktree at {dir}. It was kept because an operation is \
                 in progress.",
                dir = dir.display()
            ),
            "complete 7 Extracted the store.".to_string(),
        ];
        assert_eq!(calls(&board), expected);
        assert!(dir.join("store.rs").exists());
    }

    #[test]
    fn an_onto_task_runs_after_its_target_completed_with_ignored_build_output() {
        let (fixture, env) = setup("run-onto-after");
        fs::write(env.prompts.join("polish.md"), "You polish one task.\n").unwrap();
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let build = |_: &CommandLine, dir: &Path, log: &Path| {
            fixture.commit(dir, ".gitignore", "target/\n");
            fs::create_dir_all(dir.join("target")).unwrap();
            fs::write(dir.join("target").join("out.o"), "build output").unwrap();
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(run(&env, &board, &build), completed("Extracted the store."));
        let repo = fixture.repo.to_str().unwrap();
        let target = view(7, &[("branch", BRANCH)], TaskState::Completed);
        let labels = [("role", "polish"), ("repo", repo), ("onto", "#7")];
        *board.tasks.borrow_mut() = vec![target, view(8, &labels, TaskState::Claimed)];
        let polish = |_: &CommandLine, dir: &Path, log: &Path| {
            assert_eq!(git(dir, &["symbolic-ref", "--short", "HEAD"]), BRANCH);
            fixture.commit(dir, "polish.txt", "polished");
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        let ran = run_task(TaskId(8), &env, &board, None, &polish).unwrap();
        assert_eq!(ran, Ran::Settled(completed("Extracted the store.")));
    }

    #[test]
    fn failed_attempt_increments_the_attempt_label() {
        let (fixture, env) = setup("run-fail");
        let board = FakeBoard::new(vec![task(&fixture, &[("attempt", "1")])]);
        let logged = RefCell::new(PathBuf::new());
        let launch = |_: &CommandLine, _: &Path, log: &Path| {
            let lines: Vec<String> = (1..=50).map(|n| format!("line {n}")).collect();
            write_log(log, &(lines.join("\n") + "\n"));
            *logged.borrow_mut() = log.to_path_buf();
            Ok(Exit::Code(2))
        };
        let failed = Verdict::Fail {
            reason: "agent exited 2".to_string(),
        };
        assert_eq!(run(&env, &board, &launch), failed);
        let tail: Vec<String> = (11..=50).map(|n| format!("line {n}")).collect();
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            tail_reply(&env, 2, &tail.join("\n")),
            "set_label 7 attempt=2".to_string(),
            "fail 7 agent exited 2".to_string(),
        ];
        assert_eq!(calls(&board), expected);
        assert_eq!(
            *logged.borrow(),
            env.home.join("logs").join("7").join("2.jsonl")
        );
        assert!(worktree(&env).exists(), "a failed task keeps its worktree");
    }

    #[test]
    fn exit_zero_without_committed_work_fails() {
        let (fixture, env) = setup("run-nothing");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let idle = |_: &CommandLine, _: &Path, log: &Path| {
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        let reason = "no committed work: the branch has no commits of its own";
        let expected = Verdict::Fail {
            reason: reason.to_string(),
        };
        assert_eq!(run(&env, &board, &idle), expected);
        let last = calls(&board).last().cloned();
        assert_eq!(last, Some(format!("fail 7 {reason}")));

        let (fixture, env) = setup("run-dirty");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let dirty = |_: &CommandLine, dir: &Path, log: &Path| {
            fixture.commit(dir, "store.rs", "pub struct Store;");
            fs::write(dir.join("notes.txt"), "half done").unwrap();
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        let reason = "no committed work: the worktree has uncommitted changes";
        let expected = Verdict::Fail {
            reason: reason.to_string(),
        };
        assert_eq!(run(&env, &board, &dirty), expected);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            tail_reply(&env, 1, DONE_STREAM),
            "set_label 7 attempt=1".to_string(),
            format!("fail 7 {reason}"),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn worker_that_blocks_its_task_is_not_failed() {
        let (fixture, env) = setup("run-blocked");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let asking = |_: &CommandLine, dir: &Path, log: &Path| {
            fs::write(dir.join("notes.txt"), "which docs site?").unwrap();
            write_log(log, "{}\n");
            board.set_state(ID, TaskState::Blocked);
            Ok(Exit::Code(1))
        };
        assert_eq!(run(&env, &board, &asking), Verdict::LeaveBlocked);
        assert_eq!(
            calls(&board),
            vec![
                format!("set_label 7 branch={BRANCH}"),
                start_call(&fixture, "main")
            ]
        );
    }

    #[test]
    fn an_agent_that_cannot_be_run_fails_the_attempt() {
        let (fixture, env) = setup("run-spawn");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let broken = |_: &CommandLine, _: &Path, _: &Path| {
            Err(HarnessError::Spawn("claude: not found".to_string()))
        };
        let reason = "the agent could not be started: claude: not found";
        let expected = Verdict::Fail {
            reason: reason.to_string(),
        };
        assert_eq!(run(&env, &board, &broken), expected);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            tail_reply(&env, 1, NO_OUTPUT),
            "set_label 7 attempt=1".to_string(),
            format!("fail 7 {reason}"),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn a_retry_reuses_the_worktree_and_says_so() {
        let (fixture, env) = setup("run-retry");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let prompts = RefCell::new(Vec::new());
        let idle = |cmd: &CommandLine, _: &Path, log: &Path| {
            prompts.borrow_mut().push(cmd.args.last().cloned().unwrap());
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        run(&env, &board, &idle);
        let board = FakeBoard::new(vec![task(&fixture, &[("attempt", "1")])]);
        run(&env, &board, &idle);
        let first = format!("You implement one task.\n\n# Task 7\n\n{TEXT}");
        let second = format!("{first}\n\n{RETRY_NOTE}");
        assert_eq!(*prompts.borrow(), vec![first, second]);
    }

    #[test]
    fn a_task_that_is_not_claimed_is_not_run() {
        let (fixture, env) = setup("run-unclaimed");
        let mut ready = task(&fixture, &[]);
        ready.state = TaskState::Ready;
        let board = FakeBoard::new(vec![ready]);
        let result = run_task(ID, &env, &board, None, &never);
        assert!(
            matches!(result, Err(RunError::LostClaim(TaskState::Ready))),
            "{result:?}"
        );
        assert_eq!(calls(&board), Vec::<String>::new());
    }

    #[test]
    fn a_claim_lost_during_the_attempt_is_not_settled() {
        let (fixture, env) = setup("run-lost");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let lost = |_: &CommandLine, _: &Path, _: &Path| {
            board.set_state(ID, TaskState::Ready);
            Ok(Exit::Code(1))
        };
        let result = run_task(ID, &env, &board, None, &lost);
        assert!(
            matches!(result, Err(RunError::LostClaim(TaskState::Ready))),
            "{result:?}"
        );
        assert_eq!(
            calls(&board),
            vec![
                format!("set_label 7 branch={BRANCH}"),
                start_call(&fixture, "main")
            ]
        );
    }

    #[test]
    fn board_failures_are_returned_not_retried() {
        let (fixture, env) = setup("run-board");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        board.failing.borrow_mut().push("read_task");
        let result = run_task(ID, &env, &board, None, &never);
        assert!(matches!(result, Err(RunError::Board(_))), "{result:?}");
        assert_eq!(calls(&board), Vec::<String>::new());

        let board = FakeBoard::new(vec![task(&fixture, &[("attempt", "1")])]);
        board.failing.borrow_mut().push("fail");
        let crashed = |_: &CommandLine, _: &Path, _: &Path| Ok(Exit::Signal);
        let result = run_task(ID, &env, &board, None, &crashed);
        assert!(matches!(result, Err(RunError::Board(_))), "{result:?}");
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            tail_reply(&env, 2, NO_OUTPUT),
            "set_label 7 attempt=2".to_string(),
            "fail 7 agent exited on a signal".to_string(),
        ];
        assert_eq!(
            calls(&board),
            expected,
            "the attempt is counted before the failure"
        );
    }

    #[test]
    fn a_log_tail_that_cannot_be_posted_still_counts_and_fails_the_attempt() {
        let (fixture, env) = setup("run-tail-lost");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        board.failing.borrow_mut().push("reply");
        let crashed = |_: &CommandLine, _: &Path, _: &Path| Ok(Exit::Signal);
        let expected = Verdict::Fail {
            reason: "agent exited on a signal".to_string(),
        };
        assert_eq!(run(&env, &board, &crashed), expected);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            start_call(&fixture, "main"),
            tail_reply(&env, 1, NO_OUTPUT),
            format!("reply 7 {}", log_line(&env, 1)),
            "set_label 7 attempt=1".to_string(),
            "fail 7 agent exited on a signal".to_string(),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn each_run_of_an_attempt_keeps_its_own_log() {
        let (_fixture, env) = setup("run-logs");
        let dir = env.home.join("logs").join("7");
        assert_eq!(log_path(&env.home, ID, 1), dir.join("1.jsonl"));
        write_log(&dir.join("1.jsonl"), "first run\n");
        assert_eq!(log_path(&env.home, ID, 1), dir.join("1-2.jsonl"));
        assert_eq!(log_path(&env.home, ID, 2), dir.join("2.jsonl"));
        for run in 2..=LOG_RUNS_MAX {
            write_log(&dir.join(format!("1-{run}.jsonl")), "a run\n");
        }
        let last = dir.join(format!("1-{LOG_RUNS_MAX}.jsonl"));
        assert_eq!(
            log_path(&env.home, ID, 1),
            last,
            "at the bound the last is reused"
        );
    }

    #[test]
    fn a_blocked_then_unblocked_attempt_does_not_overwrite_its_log() {
        let (fixture, env) = setup("run-rerun");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let logs = RefCell::new(Vec::new());
        let asking = |_: &CommandLine, _: &Path, log: &Path| {
            logs.borrow_mut().push(log.to_path_buf());
            write_log(log, "{}\n");
            board.set_state(ID, TaskState::Blocked);
            Ok(Exit::Code(0))
        };
        run(&env, &board, &asking);
        board.set_state(ID, TaskState::Claimed);
        run(&env, &board, &asking);
        let dir = env.home.join("logs").join("7");
        assert_eq!(
            *logs.borrow(),
            vec![dir.join("1.jsonl"), dir.join("1-2.jsonl")]
        );
    }

    fn base_task(fixture: &Fixture) -> TaskView {
        fixture.branch_ahead("rider/3-store");
        view(3, &[("branch", "rider/3-store")], TaskState::Completed)
    }

    fn judged(env: &Env, board: &FakeBoard, launch: &Launch) -> (Verdict, Vec<String>) {
        (run(env, board, launch), calls(board))
    }

    #[test]
    fn a_commit_off_the_tasks_branch_is_not_committed_work() {
        let reason = format!("no committed work: the worktree is not on its branch {BRANCH}");
        let expected = Verdict::Fail { reason };
        let moves: [&[&str]; 2] = [
            &["switch", "-q", "-c", "elsewhere"],
            &["checkout", "-q", "--detach"],
        ];
        for (n, args) in moves.into_iter().enumerate() {
            let (fixture, env) = setup(&format!("run-off-branch-{n}"));
            let board = FakeBoard::new(vec![task(&fixture, &[])]);
            let wander = |_: &CommandLine, dir: &Path, log: &Path| {
                git(dir, args);
                fixture.commit(dir, "store.rs", "pub struct Store;");
                write_log(log, &format!("{DONE_STREAM}\n"));
                Ok(Exit::Code(0))
            };
            let (verdict, calls) = judged(&env, &board, &wander);
            assert_eq!(verdict, expected, "{args:?}");
            assert_eq!(
                calls.last(),
                Some(&format!("fail 7 {}", verdict_reason(&expected)))
            );
        }
    }

    fn verdict_reason(verdict: &Verdict) -> &str {
        match verdict {
            Verdict::Fail { reason } => reason,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn rewriting_agent(
        fixture: &Fixture,
    ) -> impl Fn(&CommandLine, &Path, &Path) -> Result<Exit, HarnessError> + '_ {
        move |_, dir, log| {
            git(dir, &["reset", "-q", "--hard", "HEAD~1"]);
            fixture.commit(dir, "store.rs", "pub struct Store;");
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        }
    }

    #[test]
    fn a_branch_rewritten_below_its_start_fails_unless_the_task_restacks() {
        let (fixture, env) = setup("run-rewrite-base");
        let base = base_task(&fixture);
        let board = FakeBoard::new(vec![task(&fixture, &[("base", "#3")]), base]);
        let reason = "no committed work: the branch no longer contains its starting commit";
        let expected = Verdict::Fail {
            reason: reason.to_string(),
        };
        assert_eq!(run(&env, &board, &rewriting_agent(&fixture)), expected);

        let (fixture, env) = setup("run-rewrite-onto");
        let base = base_task(&fixture);
        let board = FakeBoard::new(vec![task(&fixture, &[("onto", "#3")]), base]);
        let start = start_call(&fixture, "rider/3-store");
        let verdict = run(&env, &board, &rewriting_agent(&fixture));
        assert_eq!(verdict, completed("Extracted the store."));
        let head = git(&fixture.repo, &["rev-parse", "rider/3-store"]);
        let expected = vec![
            "set_label 7 branch=rider/3-store".to_string(),
            start,
            record(Some("rider/3-store"), &head, 1, "Extracted the store."),
            "complete 7 Extracted the store.".to_string(),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn a_review_that_commits_fails() {
        let (fixture, env) = setup("run-review");
        let base = base_task(&fixture);
        let mut review = task(&fixture, &[("of", "#3")]);
        review.labels[0].1 = "review".to_string();
        let board = FakeBoard::new(vec![review, base]);
        let expected = Verdict::Fail {
            reason: "review changed the checkout".to_string(),
        };
        assert_eq!(run(&env, &board, &committing_agent(&fixture)), expected);

        let (fixture, env) = setup("run-review-ok");
        let base = base_task(&fixture);
        let mut review = task(&fixture, &[("of", "#3")]);
        review.labels[0].1 = "review".to_string();
        let board = FakeBoard::new(vec![review, base]);
        let reading = |_: &CommandLine, _: &Path, log: &Path| {
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(run(&env, &board, &reading), completed("completed"));
        let head = git(&fixture.repo, &["rev-parse", "rider/3-store"]);
        let expected = vec![
            record(None, &head, 0, "completed"),
            "complete 7 completed".to_string(),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn a_retry_completes_on_work_an_earlier_attempt_committed() {
        let (fixture, env) = setup("run-earlier-work");
        let start = git(&fixture.repo, &["rev-parse", "main"]);
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let crashing = |_: &CommandLine, dir: &Path, _: &Path| {
            fixture.commit(dir, "store.rs", "pub struct Store;");
            Ok(Exit::Code(1))
        };
        let failed = Verdict::Fail {
            reason: "agent exited 1".to_string(),
        };
        assert_eq!(run(&env, &board, &crashing), failed);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            format!("set_label 7 start={start}"),
            tail_reply(&env, 1, NO_OUTPUT),
            "set_label 7 attempt=1".to_string(),
            "fail 7 agent exited 1".to_string(),
        ];
        assert_eq!(calls(&board), expected);

        let labels = [("attempt", "1"), ("start", start.as_str())];
        let board = FakeBoard::new(vec![task(&fixture, &labels)]);
        let idle = |_: &CommandLine, _: &Path, log: &Path| {
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(run(&env, &board, &idle), completed("Extracted the store."));
        let head = git(&fixture.repo, &["rev-parse", BRANCH]);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            record(Some(BRANCH), &head, 1, "Extracted the store."),
            "complete 7 Extracted the store.".to_string(),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn a_malformed_start_label_is_ignored() {
        let (fixture, env) = setup("run-bad-start");
        let board = FakeBoard::new(vec![task(&fixture, &[("start", "HEAD~3")])]);
        let idle = |_: &CommandLine, _: &Path, log: &Path| {
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        let reason = "no committed work: the branch has no commits of its own";
        let expected = Verdict::Fail {
            reason: reason.to_string(),
        };
        assert_eq!(run(&env, &board, &idle), expected);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            tail_reply(&env, 1, DONE_STREAM),
            "set_label 7 attempt=1".to_string(),
            format!("fail 7 {reason}"),
        ];
        assert_eq!(
            calls(&board),
            expected,
            "a present label is never overwritten"
        );
    }

    fn idle(_: &CommandLine, _: &Path, log: &Path) -> Result<Exit, HarnessError> {
        write_log(log, &format!("{DONE_STREAM}\n"));
        Ok(Exit::Code(0))
    }

    fn no_own_commits() -> Verdict {
        Verdict::Fail {
            reason: "no committed work: the branch has no commits of its own".to_string(),
        }
    }

    #[test]
    fn a_retry_left_off_its_branch_is_blocked_before_launch() {
        let moves: [&[&str]; 2] = [
            &["switch", "-q", "-c", "elsewhere"],
            &["checkout", "-q", "--detach"],
        ];
        let heads = ["branch elsewhere", "a detached HEAD"];
        for (n, (args, on)) in moves.into_iter().zip(heads).enumerate() {
            let (fixture, env) = setup(&format!("run-left-off-{n}"));
            let board = FakeBoard::new(vec![task(&fixture, &[])]);
            let wander = |_: &CommandLine, dir: &Path, _: &Path| {
                git(dir, args);
                fixture.commit(dir, "store.rs", "pub struct Store;");
                Ok(Exit::Code(0))
            };
            assert!(matches!(run(&env, &board, &wander), Verdict::Fail { .. }));
            let labels = [("attempt", "1"), ("branch", BRANCH)];
            let board = FakeBoard::new(vec![task(&fixture, &labels)]);
            assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
            let reason = format!(
                "Rider cannot prepare the worktree: {} is on {on}, not on branch {BRANCH}.",
                worktree(&env).display()
            );
            assert_eq!(calls(&board), vec![format!("block 7 rider_error {reason}")]);
        }
    }

    #[test]
    fn a_branch_label_naming_another_branch_is_never_overwritten() {
        let (fixture, env) = setup("run-label-kept");
        let board = FakeBoard::new(vec![task(&fixture, &[("branch", "rider/7..old")])]);
        assert_eq!(run(&env, &board, &never), Verdict::LeaveBlocked);
        let reason = format!(
            "The task's `branch` label names \"rider/7..old\" but its worktree is on branch \
             {BRANCH}; set the label to the branch that holds the task's work."
        );
        assert_eq!(calls(&board), vec![format!("block 7 rider_error {reason}")]);
    }

    #[test]
    fn a_fast_forward_onto_another_branch_is_not_work_of_its_own() {
        let (fixture, env) = setup("run-fast-forward");
        fixture.branch_ahead("sibling");
        let board = FakeBoard::new(vec![task(&fixture, &[])]);
        let borrow = |_: &CommandLine, dir: &Path, log: &Path| {
            git(dir, &["merge", "-q", "--ff-only", "sibling"]);
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(run(&env, &board, &borrow), no_own_commits());
    }

    #[test]
    fn an_older_start_label_does_not_count_others_commits() {
        let (fixture, env) = setup("run-old-start");
        let root = git(&fixture.repo, &["rev-parse", "main"]);
        fixture.commit(&fixture.repo, "later.txt", "later work on main");
        let board = FakeBoard::new(vec![task(&fixture, &[("start", root.as_str())])]);
        assert_eq!(run(&env, &board, &idle), no_own_commits());
    }

    #[test]
    fn a_start_label_naming_no_commit_here_is_ignored() {
        let (fixture, env) = setup("run-missing-start");
        let missing = "0".repeat(40);
        let board = FakeBoard::new(vec![task(&fixture, &[("start", missing.as_str())])]);
        let verdict = run(&env, &board, &committing_agent(&fixture));
        assert_eq!(verdict, completed("Extracted the store."));
        let head = git(&fixture.repo, &["rev-parse", BRANCH]);
        let expected = vec![
            format!("set_label 7 branch={BRANCH}"),
            record(Some(BRANCH), &head, 1, "Extracted the store."),
            "complete 7 Extracted the store.".to_string(),
        ];
        assert_eq!(calls(&board), expected);
    }

    #[test]
    fn a_restack_completes_when_it_rebases_and_fails_when_it_only_drops() {
        let (fixture, env) = setup("run-restack");
        let base = base_task(&fixture);
        fixture.commit(&fixture.repo, "main.txt", "main moved on");
        let board = FakeBoard::new(vec![task(&fixture, &[("onto", "#3")]), base]);
        let rebase = |_: &CommandLine, dir: &Path, log: &Path| {
            git(dir, &["rebase", "-q", "main"]);
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(
            run(&env, &board, &rebase),
            completed("Extracted the store.")
        );
        let head = git(&fixture.repo, &["rev-parse", "rider/3-store"]);
        let last = calls(&board)[2].clone();
        assert_eq!(
            last,
            record(Some("rider/3-store"), &head, 1, "Extracted the store.")
        );

        let (fixture, env) = setup("run-restack-drop");
        let base = base_task(&fixture);
        let board = FakeBoard::new(vec![task(&fixture, &[("onto", "#3")]), base]);
        let drop = |_: &CommandLine, dir: &Path, log: &Path| {
            git(dir, &["reset", "-q", "--hard", "HEAD~1"]);
            write_log(log, &format!("{DONE_STREAM}\n"));
            Ok(Exit::Code(0))
        };
        assert_eq!(run(&env, &board, &drop), no_own_commits());
    }

    #[test]
    fn a_long_line_tail_survives_an_empty_read() {
        assert_eq!(
            long_line(&[]),
            "(last line longer than 16 KiB, truncated)\n"
        );
        assert_eq!(
            long_line(b"\nab"),
            "(last line longer than 16 KiB, truncated)\nab"
        );
    }

    #[test]
    fn errors_describe_themselves_in_one_line() {
        let git_error = GitError::Exit {
            args: "status".to_string(),
            stderr: "fatal: not a\n  git repository".to_string(),
        };
        assert_eq!(
            git_error.to_string(),
            "git status failed: fatal: not a git repository"
        );
        let board_error = BoardError::Exit {
            code: Some(2),
            stderr: "no such task\n".to_string(),
        };
        assert_eq!(
            board_error.to_string(),
            "vb exited with code 2: no such task"
        );
        let worktree_error = WorktreeError::Board(board_error);
        assert_eq!(
            worktree_error.to_string(),
            "vb exited with code 2: no such task"
        );
        let harness_error = HarnessError::Log("/h/logs/7/1.jsonl: disk full".to_string());
        assert_eq!(
            harness_error.to_string(),
            "the agent's log failed: /h/logs/7/1.jsonl: disk full"
        );
        let long = GitError::Parse {
            args: "rev-parse HEAD".to_string(),
            stdout: "x ".repeat(2000),
        };
        assert!(
            long.to_string().chars().count() < 600,
            "external text is cut"
        );
    }

    #[test]
    fn worktree_errors_block_with_a_kind_the_lead_can_act_on() {
        let cases = [
            (
                WorktreeError::OntoHasNoBranch(TaskId(3)),
                "invalid_brief",
                "Set the `branch` label on task #3 to its branch, or remove the `onto` label \
              from this task.",
            ),
            (
                WorktreeError::OfHasNoBranch(TaskId(3)),
                "invalid_brief",
                "Set the `branch` label on task #3 to the branch to check, or change the `of` \
              label to a task that has one.",
            ),
            (
                WorktreeError::OntoBranchBusy(TaskId(3)),
                "branch_busy",
                "Wait until the branch of task #3 is not checked out in another worktree, \
              then unblock this task.",
            ),
            (
                WorktreeError::BranchBusy("rider/7-x".to_string()),
                "branch_busy",
                "Wait until branch rider/7-x is not checked out in another worktree, then \
              unblock this task.",
            ),
        ];
        assert_blocks(&cases);
    }

    #[test]
    fn other_worktree_errors_block_as_rider_errors() {
        let cases = [
            (
                WorktreeError::NotACheckout(PathBuf::from("/h/worktrees/repo/7")),
                "rider_error",
                "Rider cannot prepare the worktree: /h/worktrees/repo/7 exists but is not a \
              checkout of the repository.",
            ),
            (
                WorktreeError::Git(GitError::Io("waiting for git: interrupted".to_string())),
                "rider_error",
                "Rider cannot prepare the worktree: reading from git failed: waiting for git: \
                 interrupted.",
            ),
            (
                WorktreeError::Board(BoardError::TooManyLabels),
                "rider_error",
                "Rider cannot prepare the worktree: the task has more than 32 labels.",
            ),
            (
                WorktreeError::NotOnBranch {
                    dir: PathBuf::from("/h/worktrees/repo/7"),
                    on: None,
                    expected: Some("rider/7-x".to_string()),
                },
                "rider_error",
                "Rider cannot prepare the worktree: /h/worktrees/repo/7 is on a detached HEAD, \
                 not on branch rider/7-x.",
            ),
            (
                WorktreeError::NotReleasable {
                    dir: PathBuf::from("/h/worktrees/repo/7"),
                    reason: "an operation is in progress",
                },
                "rider_error",
                "Rider cannot prepare the worktree: /h/worktrees/repo/7 cannot be removed \
                 safely: an operation is in progress.",
            ),
        ];
        assert_blocks(&cases);
    }

    fn assert_blocks(cases: &[(WorktreeError, &str, &str)]) {
        for (error, kind, reason) in cases {
            let block = worktree_block(error);
            assert_eq!(
                (block.kind, block.reason.as_str()),
                (*kind, *reason),
                "{error:?}"
            );
        }
    }

    #[test]
    fn the_log_tail_is_the_last_lines_read_from_the_end() {
        let (fixture, _env) = setup("run-tail");
        let log = fixture.root.join("big.jsonl");
        assert_eq!(log_tail(&log), "");
        let line = "x".repeat(99);
        let mut text = Vec::new();
        for _ in 0..1000 {
            text.extend_from_slice(line.as_bytes());
            text.push(b'\n');
        }
        text.extend_from_slice(b"bad \xff byte\nlast");
        fs::write(&log, &text).unwrap();
        let tail = log_tail(&log);
        let mut expected = vec![line.as_str(); 38];
        expected.extend(["bad \u{fffd} byte", "last"]);
        assert_eq!(tail, expected.join("\n"));

        let window = usize::try_from(LOG_TAIL_BYTES_MAX).unwrap();
        let long = "y".repeat(window * 2) + "\nend\n";
        fs::write(&log, long).unwrap();
        assert_eq!(log_tail(&log), "end", "a partial first line is dropped");

        fs::write(&log, "z".repeat(window * 2)).unwrap();
        let expected = format!(
            "(last line longer than 16 KiB, truncated)\n{}",
            "z".repeat(window)
        );
        assert_eq!(log_tail(&log), expected);
    }
}
