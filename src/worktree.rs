//! The checkout a task runs in: its own worktree, on its own branch or detached.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::board::{Board, BoardError, LABELS_MAX, TaskId, TaskState, TaskView};
use crate::brief::Brief;
use crate::git::{self, GitError, REF_BYTES_MAX};

/// Upper bound, in bytes, on a branch name's slug.
pub const SLUG_MAX: usize = 40;
const SLUG_SCAN_CHARS_MAX: usize = 1024;
const SLUG_EMPTY: &str = "task";
const BRANCH_PREFIX: &str = "rider/";
const HEADS_PREFIX: &str = "refs/heads/";
// A branch label must still be a safe ref once `HEADS_PREFIX` makes it a full ref.
const BRANCH_BYTES_MAX: usize = REF_BYTES_MAX - HEADS_PREFIX.len();
pub const WORKTREES_DIR: &str = "worktrees";
pub const TASK_DIRS_MAX: usize = 1024;
const PROJECT_FALLBACK: &str = "project";
/// The label that holds the branch a task's work is on.
pub const BRANCH_LABEL: &str = "branch";
// In the worktree's own git directory, beside its `gitdir` link: never a checked-out file, and
// removed with the worktree.
const BOARD_RECORD: &str = ".rider-board";
const BOARD_RECORD_BYTES_MAX: u64 = 4096;
// What git keeps in a worktree's git directory while an operation is unfinished.
const IN_PROGRESS: [&str; 8] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "sequencer",
    "BISECT_LOG",
    "BISECT_START",
];

/// A task's checkout: where it is, the branch it is on, and the commit it started at.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub dir: PathBuf,
    /// `None` when the checkout is detached.
    pub branch: Option<String>,
    /// The commit HEAD points at when `prepare` returns.
    pub start_head: String,
    pub base: Option<String>,
}

/// Why a checkout could not be prepared or released.
#[derive(Debug)]
pub enum WorktreeError {
    Git(GitError),
    Board(BoardError),
    BaseHasNoBranch(TaskId),
    OntoHasNoBranch(TaskId),
    OfHasNoBranch(TaskId),
    /// The task's directory exists but is not the top of a checkout of the repository.
    NotACheckout(PathBuf),
    /// The task names no branch to cut from and the repository has no `origin/HEAD`.
    NoFromBranch,
    /// The `onto` task's branch is checked out in another worktree.
    OntoBranchBusy(TaskId),
    /// The task's own existing branch is checked out in another worktree.
    BranchBusy(String),
    /// Removing the worktree is not provably safe, for the reason given.
    NotReleasable {
        dir: PathBuf,
        reason: &'static str,
    },
    /// The checkout was made for another board, or its record of the board is missing.
    ForeignBoard {
        dir: PathBuf,
        record: Record,
    },
    Listing {
        path: PathBuf,
        why: String,
    },
    /// The worktree's record of its board could not be read or written.
    BoardRecord {
        path: PathBuf,
        why: String,
    },
    /// The checkout is not on the task's branch, or not detached when it should be; `None`
    /// stands for a detached HEAD.
    NotOnBranch {
        dir: PathBuf,
        on: Option<String>,
        expected: Option<String>,
    },
}

/// What a reused worktree's record of its board was found to be, when it is not this board's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    Missing,
    Different,
}

/// Which board a worktree is made for: the identity the board keeps in its own data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardIdentity(String);

impl BoardIdentity {
    /// The identity a board reported, as its worktrees record it.
    pub fn new(marker: &str) -> BoardIdentity {
        assert!(!marker.is_empty() && !marker.contains('\n'));
        let identity = BoardIdentity(format!("board {marker}\n"));
        assert!(u64::try_from(identity.0.len()).is_ok_and(|n| n <= BOARD_RECORD_BYTES_MAX));
        identity
    }
}

impl fmt::Display for WorktreeError {
    /// One line saying what is wrong with the checkout.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::Git(error) => write!(f, "{error}"),
            WorktreeError::Board(error) => write!(f, "{error}"),
            WorktreeError::BaseHasNoBranch(task) => {
                write!(f, "base task #{} has no usable `branch` label", task.0)
            }
            WorktreeError::OntoHasNoBranch(task) => {
                write!(f, "onto task #{} has no usable `branch` label", task.0)
            }
            WorktreeError::OfHasNoBranch(task) => {
                write!(f, "reviewed task #{} has no usable `branch` label", task.0)
            }
            WorktreeError::NotACheckout(dir) => write!(
                f,
                "{} exists but is not a checkout of the repository",
                dir.display()
            ),
            WorktreeError::NoFromBranch => {
                write!(
                    f,
                    "no branch to cut from: no `from` label and no origin/HEAD"
                )
            }
            WorktreeError::OntoBranchBusy(task) => write!(
                f,
                "the branch of onto task #{} is checked out in another worktree",
                task.0
            ),
            WorktreeError::BranchBusy(branch) => {
                write!(f, "branch {branch} is checked out in another worktree")
            }
            WorktreeError::NotReleasable { dir, reason } => {
                write!(f, "{} cannot be removed safely: {reason}", dir.display())
            }
            WorktreeError::ForeignBoard { dir, record } => {
                let found = match record {
                    Record::Missing => "it has no .rider-board record",
                    Record::Different => "its .rider-board record names another board",
                };
                write!(f, "{} was not made for this board: {found}", dir.display())
            }
            WorktreeError::Listing { path, why } => {
                write!(f, "could not list {}: {why}", path.display())
            }
            WorktreeError::BoardRecord { path, why } => {
                write!(f, "the board record {} failed: {why}", path.display())
            }
            WorktreeError::NotOnBranch { dir, on, expected } => {
                let head = |branch: &Option<String>| match branch {
                    Some(branch) => format!("branch {branch}"),
                    None => "a detached HEAD".to_string(),
                };
                let (on, expected) = (head(on), head(expected));
                write!(f, "{} is on {on}, not on {expected}", dir.display())
            }
        }
    }
}

impl From<GitError> for WorktreeError {
    fn from(error: GitError) -> WorktreeError {
        WorktreeError::Git(error)
    }
}

impl From<BoardError> for WorktreeError {
    fn from(error: BoardError) -> WorktreeError {
        WorktreeError::Board(error)
    }
}

enum Plan {
    NewBranch { branch: String, from: String },
    Onto { task: TaskId, branch: String },
    Detached(String),
}

/// The first line of `text` as a lowercase `[a-z0-9-]` slug of at most `SLUG_MAX` bytes.
pub fn slug(text: &str) -> String {
    let first = text.lines().next().unwrap_or("");
    let mut slug = String::with_capacity(SLUG_MAX);
    let mut gap = false;
    for c in first.chars().take(SLUG_SCAN_CHARS_MAX) {
        if !c.is_ascii_alphanumeric() {
            gap = true;
            continue;
        }
        let dash = gap && !slug.is_empty();
        if slug.len() + usize::from(dash) + 1 > SLUG_MAX {
            break;
        }
        if dash {
            slug.push('-');
        }
        slug.push(c.to_ascii_lowercase());
        gap = false;
    }
    if slug.is_empty() {
        slug.push_str(SLUG_EMPTY);
    }
    assert!(slug.len() <= SLUG_MAX && !slug.starts_with('-') && !slug.ends_with('-'));
    let allowed = |b: u8| b == b'-' || b.is_ascii_lowercase() || b.is_ascii_digit();
    assert!(slug.bytes().all(allowed));
    slug
}

// The label is external input: one that is absent, unsafe, too long for a full ref, or that
// names no local branch of `repo` (a tag or `HEAD`, say) is `missing(id)`.
fn linked_branch(
    board: &dyn Board,
    repo: &Path,
    id: TaskId,
    missing: fn(TaskId) -> WorktreeError,
) -> Result<String, WorktreeError> {
    assert!(repo.is_absolute());
    let view = board.read_task(id)?;
    if view.id != id {
        let reason = format!("asked for task {}, read task {}", id.0, view.id.0);
        return Err(WorktreeError::Board(BoardError::Parse(reason)));
    }
    assert!(
        view.labels.len() <= LABELS_MAX,
        "a task view's labels are bounded"
    );
    let Some(branch) = branch_label(&view) else {
        return Err(missing(id));
    };
    if !git::branch_exists(repo, branch)? {
        return Err(missing(id));
    }
    assert!(branch.len() <= BRANCH_BYTES_MAX);
    Ok(branch.to_string())
}

// The task's `branch` label, when it is safe to use as a branch name.
fn branch_label(view: &TaskView) -> Option<&str> {
    assert!(
        view.labels.len() <= LABELS_MAX,
        "a task view's labels are bounded"
    );
    // The last duplicate wins, as it does for a brief's labels.
    let labels = view.labels.iter().take(LABELS_MAX).rev();
    let mut values = labels.filter(|(key, _)| key == BRANCH_LABEL);
    let value = values.next().map(|(_, value)| value.as_str());
    let branch = value.filter(|v| v.len() <= BRANCH_BYTES_MAX && git::is_safe_ref(v));
    assert!(branch.is_none_or(|b| !b.is_empty()));
    branch
}

fn planned_branch(brief: &Brief) -> String {
    let branch = format!("{BRANCH_PREFIX}{}-{}", brief.id.0, slug(&brief.text));
    assert!(git::is_safe_ref(&branch));
    assert!(branch.len() <= BRANCH_BYTES_MAX);
    branch
}

// A task's branch is fixed when it is first prepared: its own label names it from then on,
// even after its worktree is released and its text, and so its planned name, changes.
fn expected_branch(brief: &Brief, board: &dyn Board) -> Result<String, WorktreeError> {
    assert!(brief.role.commits_expected);
    let view = board.read_task(brief.id)?;
    if view.id != brief.id {
        let reason = format!("asked for task {}, read task {}", brief.id.0, view.id.0);
        return Err(WorktreeError::Board(BoardError::Parse(reason)));
    }
    let branch = match (branch_label(&view), brief.onto) {
        (Some(own), _) => own.to_string(),
        (None, Some(task)) => {
            linked_branch(board, &brief.repo, task, WorktreeError::OntoHasNoBranch)?
        }
        (None, None) => planned_branch(brief),
    };
    assert!(git::is_safe_ref(&branch));
    Ok(branch)
}

fn full_ref(branch: &str) -> String {
    assert!(branch.len() <= BRANCH_BYTES_MAX && git::is_safe_ref(branch));
    let full = format!("{HEADS_PREFIX}{branch}");
    assert!(git::is_safe_ref(&full));
    full
}

fn plan(brief: &Brief, board: &dyn Board) -> Result<Plan, WorktreeError> {
    let repo = brief.repo.as_path();
    if !brief.role.commits_expected {
        let named = "a parsed brief for a role without commits names its task";
        assert!(brief.of.is_some(), "{named}");
        let Some(of) = brief.of else {
            return Err(WorktreeError::OfHasNoBranch(brief.id));
        };
        let branch = linked_branch(board, repo, of, WorktreeError::OfHasNoBranch)?;
        return Ok(Plan::Detached(branch));
    }
    assert!(brief.base.is_none() || brief.onto.is_none());
    if let Some(task) = brief.onto {
        let branch = linked_branch(board, repo, task, WorktreeError::OntoHasNoBranch)?;
        return Ok(Plan::Onto { task, branch });
    }
    let from = match (brief.base, &brief.from) {
        (Some(base), _) => {
            let branch = linked_branch(board, repo, base, WorktreeError::BaseHasNoBranch)?;
            full_ref(&branch)
        }
        (None, Some(from)) => from.clone(),
        (None, None) => git::default_branch(repo)?.ok_or(WorktreeError::NoFromBranch)?,
    };
    let branch = expected_branch(brief, board)?;
    assert!(git::is_safe_ref(&from));
    Ok(Plan::NewBranch { branch, from })
}

fn base_of(plan: &Plan) -> Option<String> {
    let Plan::NewBranch { from, .. } = plan else {
        return None;
    };
    let base = from.strip_prefix(HEADS_PREFIX).unwrap_or(from);
    assert!(!base.is_empty() && git::is_safe_ref(base));
    Some(base.to_string())
}

fn add(repo: &Path, dir: &Path, plan: Plan) -> Result<Option<String>, WorktreeError> {
    assert!(dir.is_absolute() && repo.is_absolute());
    let branch = match plan {
        Plan::NewBranch { branch, from } => {
            if !git::branch_exists(repo, &branch)? {
                let commit = git::commit_of(repo, &from)?;
                git::add_on_new_branch(repo, dir, &branch, &commit)?;
            } else if git::worktree_branches(repo)?
                .iter()
                .any(|(_, held)| *held == branch)
            {
                return Err(WorktreeError::BranchBusy(branch));
            } else {
                git::add_on_branch(repo, dir, &branch)?;
            }
            Some(branch)
        }
        Plan::Onto { task, branch } => {
            if git::worktree_branches(repo)?
                .iter()
                .any(|(_, held)| *held == branch)
            {
                return Err(WorktreeError::OntoBranchBusy(task));
            }
            git::add_on_branch(repo, dir, &branch)?;
            Some(branch)
        }
        Plan::Detached(branch) => {
            let commit = git::commit_of(repo, &full_ref(&branch))?;
            git::add_detached(repo, dir, &commit)?;
            None
        }
    };
    let on = git::current_branch(dir)?;
    if on != branch {
        let dir = dir.to_path_buf();
        return Err(WorktreeError::NotOnBranch {
            dir,
            on,
            expected: branch,
        });
    }
    assert!(branch.as_deref().is_none_or(git::is_safe_ref));
    Ok(branch)
}

// A path that cannot be resolved is the same as nothing.
fn same_dir(a: &Path, b: &Path) -> bool {
    assert!(a.is_absolute() && b.is_absolute());
    let same = match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    assert!(
        !same || a.exists(),
        "only an existing directory is the same as another"
    );
    same
}

fn is_checkout_of(dir: &Path, repo: &Path) -> Result<bool, WorktreeError> {
    assert!(dir.is_absolute() && repo.is_absolute());
    if !git::is_toplevel(dir)? {
        return Ok(false);
    }
    let ours = git::common_dir(dir)?;
    let same = same_dir(&ours, &git::common_dir(repo)?);
    assert!(ours.is_absolute());
    Ok(same)
}

fn record_path(dir: &Path) -> Result<PathBuf, WorktreeError> {
    let path = git::git_dir(dir)?.join(BOARD_RECORD);
    assert!(path.is_absolute() && path.ends_with(BOARD_RECORD));
    Ok(path)
}

fn recorded_board(dir: &Path) -> Result<Option<String>, WorktreeError> {
    let path = record_path(dir)?;
    let failed = |error: std::io::Error| WorktreeError::BoardRecord {
        path: path.clone(),
        why: error.to_string(),
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed(error)),
    };
    let mut bytes = Vec::new();
    let read = file
        .take(BOARD_RECORD_BYTES_MAX + 1)
        .read_to_end(&mut bytes);
    read.map_err(failed)?;
    assert!(u64::try_from(bytes.len()).is_ok_and(|n| n <= BOARD_RECORD_BYTES_MAX + 1));
    // A record too long or not UTF-8 is not this board's, whose record is short text.
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn reuse(
    brief: &Brief,
    board: &dyn Board,
    dir: PathBuf,
    identity: &BoardIdentity,
) -> Result<Checkout, WorktreeError> {
    assert!(dir.is_absolute());
    if !is_checkout_of(&dir, &brief.repo)? {
        return Err(WorktreeError::NotACheckout(dir));
    }
    let record = match recorded_board(&dir)? {
        None => Some(Record::Missing),
        Some(found) if found != identity.0 => Some(Record::Different),
        Some(_) => None,
    };
    if let Some(record) = record {
        return Err(WorktreeError::ForeignBoard { dir, record });
    }
    let expected = match brief.role.commits_expected {
        true => Some(expected_branch(brief, board)?),
        false => None,
    };
    let branch = git::current_branch(&dir)?;
    if branch != expected {
        return Err(WorktreeError::NotOnBranch {
            dir,
            on: branch,
            expected,
        });
    }
    let start_head = git::head(&dir)?;
    assert!(brief.role.commits_expected || branch.is_none());
    assert!(branch.as_deref().is_none_or(|b| !b.is_empty()));
    let base = plan(brief, board).ok().as_ref().and_then(base_of);
    Ok(Checkout {
        dir,
        branch,
        start_head,
        base,
    })
}

/// Where the task's checkout lives under `home`, whether or not it exists yet.
pub fn dir_for(brief: &Brief, home: &Path) -> PathBuf {
    assert!(home.is_absolute(), "rider's home is an absolute path");
    assert!(
        brief.repo.is_absolute(),
        "a parsed brief's repo is absolute"
    );
    let project = brief.repo.file_name().filter(|name| !name.is_empty());
    let project = project.unwrap_or(OsStr::new(PROJECT_FALLBACK));
    assert!(
        Path::new(project).components().count() == 1,
        "one directory, not a path"
    );
    let id = brief.id.0.to_string();
    home.join(WORKTREES_DIR).join(project).join(id)
}

/// The task's checkout under `home`, reused exactly as found when it was made for the board
/// `identity` names, or else added as the brief says and marked as made for that board.
pub fn prepare(
    brief: &Brief,
    board: &dyn Board,
    home: &Path,
    identity: &BoardIdentity,
) -> Result<Checkout, WorktreeError> {
    let dir = dir_for(brief, home);
    assert!(dir.starts_with(home) && dir != home);
    // Anything at the path counts, even a dangling symlink: it is never replaced.
    if fs::symlink_metadata(&dir).is_ok() {
        return reuse(brief, board, dir, identity);
    }
    let plan = plan(brief, board)?;
    if let Plan::Onto { branch, .. } = &plan {
        let holders = git::worktree_branches(&brief.repo)?;
        let held = holders.into_iter().find(|(_, held)| held == branch);
        let project = dir.parent().filter(|project| project.exists());
        if let Some((held, _)) = held
            && let Some(project) = project
            && held
                .parent()
                .is_some_and(|parent| parent.is_absolute() && same_dir(parent, project))
            && let Some(owner) = held.file_name().and_then(|name| {
                let name = name.to_str()?;
                name.parse::<u64>().ok().filter(|id| id.to_string() == name)
            })
            && recorded_board(&held).ok().flatten().as_deref() == Some(identity.0.as_str())
            && board.read_task(TaskId(owner))?.state == TaskState::Completed
        {
            let left = Checkout {
                dir: held,
                branch: Some(branch.clone()),
                start_head: String::new(),
                base: None,
            };
            let _ = release(&left, &brief.repo, home);
        }
    }
    let base = base_of(&plan);
    let branch = add(&brief.repo, &dir, plan)?;
    let path = record_path(&dir)?;
    fs::write(&path, &identity.0).map_err(|error| WorktreeError::BoardRecord {
        path,
        why: error.to_string(),
    })?;
    let start_head = git::head(&dir)?;
    assert!(brief.role.commits_expected == branch.is_some());
    Ok(Checkout {
        dir,
        branch,
        start_head,
        base,
    })
}

/// Removes a finished task's worktree only when that is provably safe; its branch is kept.
pub fn release(checkout: &Checkout, repo: &Path, home: &Path) -> Result<(), WorktreeError> {
    assert!(home.is_absolute(), "rider's home is an absolute path");
    assert!(checkout.dir.is_absolute() && repo.is_absolute());
    let refuse = |reason| WorktreeError::NotReleasable {
        dir: checkout.dir.clone(),
        reason,
    };
    let root = fs::canonicalize(home.join(WORKTREES_DIR));
    let root = root.map_err(|_| refuse("rider's worktrees directory is missing"))?;
    let link = fs::symlink_metadata(&checkout.dir).map_err(|_| refuse("it does not exist"))?;
    if link.file_type().is_symlink() {
        return Err(refuse("it is a symlink"));
    }
    let dir = fs::canonicalize(&checkout.dir).map_err(|_| refuse("it does not exist"))?;
    if dir == root || !dir.starts_with(&root) {
        return Err(refuse("it is outside rider's worktrees"));
    }
    if !is_checkout_of(&dir, repo)? {
        return Err(refuse("it is not a checkout of the repository"));
    }
    let listed = git::worktree_dirs(repo)?;
    if !listed.iter().any(|listed| same_dir(listed, &dir)) {
        return Err(refuse(
            "git does not list it as a worktree of the repository",
        ));
    }
    let git_dir = git::git_dir(&dir)?;
    if same_dir(&git_dir, &git::common_dir(&dir)?) {
        return Err(refuse("it is the main checkout"));
    }
    let busy = IN_PROGRESS
        .iter()
        .any(|name| fs::symlink_metadata(git_dir.join(name)).is_ok());
    if busy {
        return Err(refuse("an operation is in progress"));
    }
    // Commits made away from the expected branch would become unreachable.
    if git::current_branch(&dir)? != checkout.branch {
        return Err(refuse("not on its branch"));
    }
    if checkout.branch.is_none() && git::head(&dir)? != checkout.start_head {
        return Err(refuse("moved from its start"));
    }
    if let Some(reason) = work_lost_by_removal(&dir)? {
        return Err(refuse(reason));
    }
    git::remove_worktree(repo, &dir)?;
    assert!(dir.starts_with(&root) && dir != root);
    Ok(())
}

fn work_lost_by_removal(dir: &Path) -> Result<Option<&'static str>, WorktreeError> {
    assert!(dir.is_absolute());
    let reason = if !git::is_clean(dir)? {
        Some("it has modified or untracked files")
    } else if git::has_hidden_index_entries(dir)? {
        Some("a tracked file is marked assume-unchanged or skip-worktree")
    } else if git::has_per_worktree_refs(dir)? {
        Some("it holds per-worktree refs")
    } else if !git::keeps_refs_in_files(dir)? {
        Some("its repository does not keep refs in files, so its HEAD reflog cannot be checked")
    // Removing the worktree deletes its HEAD reflog; branch reflogs live on in the repository.
    } else if git::head_reflog_only_commit_count(dir)? != 0 {
        Some("commits only its HEAD reflog reaches")
    } else if git::orig_head_only_commit_count(dir)? != 0 {
        Some("commits only its ORIG_HEAD reaches")
    } else {
        None
    };
    assert!(reason.is_none_or(|reason| !reason.is_empty()));
    Ok(reason)
}

fn entries(dir: &Path) -> Result<Vec<PathBuf>, WorktreeError> {
    let failed = |why: String| WorktreeError::Listing {
        path: dir.to_path_buf(),
        why,
    };
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(failed(error.to_string())),
    };
    let mut paths = Vec::new();
    for entry in read {
        if paths.len() == TASK_DIRS_MAX {
            return Err(failed(format!(
                "it holds more than {TASK_DIRS_MAX} entries"
            )));
        }
        paths.push(entry.map_err(|error| failed(error.to_string()))?.path());
    }
    paths.sort_unstable();
    assert!(paths.len() <= TASK_DIRS_MAX);
    assert!(paths.iter().all(|path| path.starts_with(dir)));
    Ok(paths)
}

pub fn task_dirs(home: &Path) -> Result<Vec<(PathBuf, TaskId)>, WorktreeError> {
    assert!(home.is_absolute(), "rider's home is an absolute path");
    let mut found = Vec::new();
    for project in entries(&home.join(WORKTREES_DIR))? {
        if !project.is_dir() {
            continue;
        }
        for dir in entries(&project)? {
            let name = dir.file_name().and_then(OsStr::to_str);
            let id = name.filter(|name| name.bytes().all(|b| b.is_ascii_digit()));
            if let Some(id) = id.and_then(|id| id.parse::<u64>().ok()) {
                found.push((dir, TaskId(id)));
            }
        }
    }
    assert!(found.iter().all(|(dir, _)| dir.starts_with(home)));
    Ok(found)
}

pub fn made_for(dir: &Path, identity: &BoardIdentity) -> Result<bool, WorktreeError> {
    assert!(dir.is_absolute());
    let made = recorded_board(dir)?.is_some_and(|found| found == identity.0);
    assert!(!made || dir.exists());
    Ok(made)
}

pub fn release_ended(brief: &Brief, view: &TaskView, home: &Path) -> Result<(), WorktreeError> {
    assert!(brief.id == view.id);
    let dir = dir_for(brief, home);
    let branch = match brief.role.commits_expected {
        true => match branch_label(view) {
            Some(branch) => Some(branch.to_string()),
            None => {
                let reason = "its task has no usable `branch` label";
                return Err(WorktreeError::NotReleasable { dir, reason });
            }
        },
        false => None,
    };
    let start_head = git::head(&dir)?;
    let checkout = Checkout {
        dir,
        branch,
        start_head,
        base: None,
    };
    release(&checkout, &brief.repo, home)
}
