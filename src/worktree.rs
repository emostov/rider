//! The checkout a task runs in: its own worktree, on its own branch or detached.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::board::{Board, BoardError, LABELS_MAX, TaskId, TaskView};
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
const WORKTREES_DIR: &str = "worktrees";
const PROJECT_FALLBACK: &str = "project";
/// The label that holds the branch a task's work is on.
pub const BRANCH_LABEL: &str = "branch";
// What git keeps in a worktree's git directory while an operation is unfinished.
const IN_PROGRESS: [&str; 7] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
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
    /// The checkout is not on the task's branch, or not detached when it should be; `None`
    /// stands for a detached HEAD.
    NotOnBranch {
        dir: PathBuf,
        on: Option<String>,
        expected: Option<String>,
    },
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

// A task's branch is fixed when it is first prepared: its own label names it from then on.
fn expected_branch(brief: &Brief, board: &dyn Board) -> Result<Option<String>, WorktreeError> {
    if !brief.role.commits_expected {
        return Ok(None);
    }
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
    Ok(Some(branch))
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
    let branch = planned_branch(brief);
    assert!(git::is_safe_ref(&from));
    Ok(Plan::NewBranch { branch, from })
}

fn add(repo: &Path, dir: &Path, plan: Plan) -> Result<Option<String>, WorktreeError> {
    assert!(dir.is_absolute() && repo.is_absolute());
    let branch = match plan {
        Plan::NewBranch { branch, from } => {
            if !git::branch_exists(repo, &branch)? {
                let commit = git::commit_of(repo, &from)?;
                git::add_on_new_branch(repo, dir, &branch, &commit)?;
            } else if git::worktree_branches(repo)?.contains(&branch) {
                return Err(WorktreeError::BranchBusy(branch));
            } else {
                git::add_on_branch(repo, dir, &branch)?;
            }
            Some(branch)
        }
        Plan::Onto { task, branch } => {
            if git::worktree_branches(repo)?.contains(&branch) {
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

fn reuse(brief: &Brief, board: &dyn Board, dir: PathBuf) -> Result<Checkout, WorktreeError> {
    assert!(dir.is_absolute());
    if !is_checkout_of(&dir, &brief.repo)? {
        return Err(WorktreeError::NotACheckout(dir));
    }
    let expected = expected_branch(brief, board)?;
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
    Ok(Checkout {
        dir,
        branch,
        start_head,
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

/// The task's checkout under `home`, reused exactly as found or else added as the brief says.
pub fn prepare(brief: &Brief, board: &dyn Board, home: &Path) -> Result<Checkout, WorktreeError> {
    let dir = dir_for(brief, home);
    assert!(dir.starts_with(home) && dir != home);
    // Anything at the path counts, even a dangling symlink: it is never replaced.
    if fs::symlink_metadata(&dir).is_ok() {
        return reuse(brief, board, dir);
    }
    let plan = plan(brief, board)?;
    let branch = add(&brief.repo, &dir, plan)?;
    let start_head = git::head(&dir)?;
    assert!(brief.role.commits_expected == branch.is_some());
    Ok(Checkout {
        dir,
        branch,
        start_head,
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
    let dir = fs::canonicalize(&checkout.dir).map_err(|_| refuse("it does not exist"))?;
    if dir == root || !dir.starts_with(&root) {
        return Err(refuse("it is outside rider's worktrees"));
    }
    if !is_checkout_of(&dir, repo)? {
        return Err(refuse("it is not a checkout of the repository"));
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
    git::remove_worktree(repo, &dir)?;
    assert!(dir.starts_with(&root) && dir != root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::testing::FakeBoard;
    use crate::board::{TaskState, TaskView};
    use crate::git::testing::{Fixture, git, git_succeeds};
    use crate::roles::role_from_label;
    use std::fs;

    const BASE_BRANCH: &str = "rider/3-token-store";

    fn task(id: u64, branch: Option<&str>) -> TaskView {
        let labels = branch.map(|b| ("branch".to_string(), b.to_string()));
        TaskView {
            id: TaskId(id),
            text: "Earlier work.".to_string(),
            state: TaskState::Completed,
            labels: labels.into_iter().collect(),
        }
    }

    fn brief(fixture: &Fixture, role: &str, text: &str) -> Brief {
        Brief {
            id: TaskId(7),
            role: role_from_label(role).unwrap(),
            repo: fixture.repo.clone(),
            base: None,
            onto: None,
            of: None,
            from: Some("main".to_string()),
            resume: false,
            text: text.to_string(),
        }
    }

    fn expected_dir(fixture: &Fixture) -> PathBuf {
        fixture.home.join("worktrees").join("repo").join("7")
    }

    fn current_branch(dir: &Path) -> Option<String> {
        let attached = git_succeeds(dir, &["symbolic-ref", "-q", "HEAD"]);
        attached.then(|| git(dir, &["symbolic-ref", "--short", "HEAD"]))
    }

    #[test]
    fn slug_is_bounded_and_safe() {
        assert_eq!(slug("Extract Token Store!\nmore"), "extract-token-store");
        assert!(slug(&"x".repeat(200)).len() <= SLUG_MAX);
        assert_eq!(slug("!!!"), "task");
        assert_eq!(slug(""), "task");
        assert_eq!(slug("  --Fix  the__bug--  "), "fix-the-bug");
        assert_eq!(slug("Café au lait"), "caf-au-lait");
        let long = slug(&"ab ".repeat(100));
        assert!(long.len() <= SLUG_MAX && !long.ends_with('-'));
    }

    #[test]
    fn new_task_gets_a_branch_cut_from_the_default_branch() {
        let fixture = Fixture::new("wt-new");
        let main = git(&fixture.repo, &["rev-parse", "main"]);
        fixture.set_origin_head("main");
        git(&fixture.repo, &["checkout", "-q", "-b", "elsewhere"]);
        fixture.commit(&fixture.repo, "elsewhere.txt", "elsewhere");
        let board = FakeBoard::new(Vec::new());
        let mut brief = brief(&fixture, "implement", "Extract token store\nDetails.");
        brief.from = None;
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(checkout.dir, expected_dir(&fixture));
        let branch = "rider/7-extract-token-store";
        assert_eq!(checkout.branch.as_deref(), Some(branch));
        assert_eq!(checkout.start_head, main);
        assert_eq!(current_branch(&checkout.dir).as_deref(), Some(branch));
        assert!(
            board.calls.borrow().is_empty(),
            "preparing writes nothing to the board"
        );
    }

    #[test]
    fn new_task_is_cut_from_the_from_branch() {
        let fixture = Fixture::new("wt-from");
        let other = fixture.branch_ahead("feature/other");
        let board = FakeBoard::new(Vec::new());
        let mut brief = brief(&fixture, "implement", "Use other");
        brief.from = Some("feature/other".to_string());
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(checkout.start_head, other);
        assert_eq!(checkout.branch.as_deref(), Some("rider/7-use-other"));
    }

    #[test]
    fn stacked_task_is_cut_from_its_base_tasks_branch() {
        let fixture = Fixture::new("wt-base");
        let base_head = fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let mut brief = brief(&fixture, "implement", "Use the store");
        brief.base = Some(TaskId(3));
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(checkout.start_head, base_head);
        assert_eq!(checkout.branch.as_deref(), Some("rider/7-use-the-store"));
        let branch_head = git(&fixture.repo, &["rev-parse", "rider/7-use-the-store"]);
        assert_eq!(branch_head, base_head);
        assert_eq!(git(&fixture.repo, &["rev-parse", BASE_BRANCH]), base_head);
    }

    #[test]
    fn onto_task_checks_out_the_existing_branch() {
        let fixture = Fixture::new("wt-onto");
        let base_head = fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let mut brief = brief(&fixture, "polish", "Polish the store");
        brief.onto = Some(TaskId(3));
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(checkout.dir, expected_dir(&fixture));
        assert_eq!(checkout.branch.as_deref(), Some(BASE_BRANCH));
        assert_eq!(checkout.start_head, base_head);
        assert_eq!(current_branch(&checkout.dir).as_deref(), Some(BASE_BRANCH));
        assert!(!git_succeeds(
            &fixture.repo,
            &["rev-parse", "--verify", "rider/7-polish-the-store"]
        ));
    }

    #[test]
    fn review_task_is_detached_at_the_reviewed_branch_head() {
        let fixture = Fixture::new("wt-review");
        let base_head = fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let mut brief = brief(&fixture, "review", "Review the store");
        brief.of = Some(TaskId(3));
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(checkout.dir, expected_dir(&fixture));
        assert_eq!(checkout.branch, None);
        assert_eq!(checkout.start_head, base_head);
        assert_eq!(current_branch(&checkout.dir), None);
        assert_eq!(git(&checkout.dir, &["rev-parse", "HEAD"]), base_head);
    }

    #[test]
    fn existing_dirty_worktree_is_reused_untouched() {
        let fixture = Fixture::new("wt-dirty");
        let board = FakeBoard::new(vec![task(7, Some("rider/7-first"))]);
        let first = prepare(
            &brief(&fixture, "implement", "First"),
            &board,
            &fixture.home,
        );
        let first = first.unwrap();
        let committed = fixture.commit(&first.dir, "work.txt", "work in progress");
        fs::write(first.dir.join("README"), "edited, not committed\n").unwrap();
        fs::write(first.dir.join("scratch.txt"), "untracked\n").unwrap();
        let again = prepare(
            &brief(&fixture, "implement", "Renamed"),
            &board,
            &fixture.home,
        );
        let again = again.unwrap();
        assert_eq!(again.dir, first.dir);
        assert_eq!(again.branch.as_deref(), Some("rider/7-first"));
        assert_eq!(again.start_head, committed);
        let readme = fs::read_to_string(again.dir.join("README")).unwrap();
        assert_eq!(readme, "edited, not committed\n");
        assert!(again.dir.join("scratch.txt").exists());
        assert!(!crate::git::is_clean(&again.dir).unwrap());
        assert!(!git_succeeds(
            &fixture.repo,
            &["rev-parse", "--verify", "rider/7-renamed"]
        ));
    }

    fn reused_on(fixture: &Fixture, board: &FakeBoard, role: &str, args: &[&str]) -> WorktreeError {
        let mut brief = brief(fixture, role, "First");
        brief.of = (role == "review").then_some(TaskId(3));
        let first = prepare(&brief, board, &fixture.home).unwrap();
        git(&first.dir, args);
        match prepare(&brief, board, &fixture.home) {
            Err(error) => error,
            Ok(checkout) => panic!("reused {checkout:?} after git {args:?}"),
        }
    }

    fn not_on_branch(error: WorktreeError) -> (Option<String>, Option<String>) {
        match error {
            WorktreeError::NotOnBranch { on, expected, .. } => (on, expected),
            other => panic!("expected NotOnBranch, got {other:?}"),
        }
    }

    #[test]
    fn a_reused_worktree_must_be_on_the_tasks_branch() {
        let expected = Some("rider/7-first".to_string());
        let fixture = Fixture::new("wt-elsewhere");
        let board = FakeBoard::new(vec![task(7, None)]);
        let error = reused_on(
            &fixture,
            &board,
            "implement",
            &["switch", "-q", "-c", "other"],
        );
        assert_eq!(
            error.to_string(),
            format!(
                "{} is on branch other, not on branch rider/7-first",
                expected_dir(&fixture).display()
            )
        );
        assert_eq!(
            not_on_branch(error),
            (Some("other".to_string()), expected.clone())
        );
        let fixture = Fixture::new("wt-detached-again");
        let board = FakeBoard::new(vec![task(7, Some("rider/7-first"))]);
        let error = reused_on(
            &fixture,
            &board,
            "implement",
            &["checkout", "-q", "--detach"],
        );
        assert_eq!(not_on_branch(error), (None, expected));
    }

    #[test]
    fn a_reused_review_checkout_must_still_be_detached() {
        let fixture = Fixture::new("wt-review-attached");
        fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let error = reused_on(&fixture, &board, "review", &["switch", "-q", "-c", "notes"]);
        assert_eq!(not_on_branch(error), (Some("notes".to_string()), None));
    }

    #[test]
    fn release_is_not_confused_by_a_tag_named_like_the_branch() {
        let fixture = Fixture::new("wt-tag");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(
            &brief(&fixture, "implement", "Tagged"),
            &board,
            &fixture.home,
        );
        let checkout = checkout.unwrap();
        git(&fixture.repo, &["tag", "rider/7-tagged"]);
        released(&fixture, &checkout).unwrap();
        assert!(!checkout.dir.exists());
    }

    #[test]
    fn existing_review_checkout_is_reused_untouched() {
        let fixture = Fixture::new("wt-review-again");
        fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let mut brief = brief(&fixture, "review", "Review");
        brief.of = Some(TaskId(3));
        let first = prepare(&brief, &board, &fixture.home).unwrap();
        fs::write(first.dir.join("notes.txt"), "draft\n").unwrap();
        let moved = fixture.commit(&first.dir, "notes.txt", "notes");
        let again = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(again.dir, first.dir);
        assert_eq!(again.branch, None);
        assert_eq!(again.start_head, moved);
    }

    #[test]
    fn existing_branch_without_worktree_is_reattached() {
        let fixture = Fixture::new("wt-reattach");
        let branch = "rider/7-extract-token-store";
        let ahead = fixture.branch_ahead(branch);
        let board = FakeBoard::new(Vec::new());
        let brief = brief(&fixture, "implement", "Extract token store");
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        assert_eq!(checkout.branch.as_deref(), Some(branch));
        assert_eq!(checkout.start_head, ahead);
        assert_eq!(current_branch(&checkout.dir).as_deref(), Some(branch));
        assert_eq!(git(&fixture.repo, &["rev-parse", branch]), ahead);
    }

    #[test]
    fn base_without_a_branch_label_is_an_error() {
        let fixture = Fixture::new("wt-nobranch");
        fixture.branch_ahead(BASE_BRANCH);
        let bad = ["-x", "a b", "a..b", "", "line\nbreak"];
        let mut tasks = vec![task(3, None)];
        for (n, value) in (10..).zip(bad) {
            tasks.push(task(n, Some(value)));
        }
        let board = FakeBoard::new(tasks);
        for n in [3, 10, 11, 12, 13, 14] {
            let mut stacked = brief(&fixture, "implement", "Stacked");
            stacked.base = Some(TaskId(n));
            let result = prepare(&stacked, &board, &fixture.home);
            assert!(matches!(result, Err(WorktreeError::BaseHasNoBranch(TaskId(id))) if id == n));
            let mut onto = brief(&fixture, "polish", "Onto");
            onto.onto = Some(TaskId(n));
            let result = prepare(&onto, &board, &fixture.home);
            assert!(matches!(result, Err(WorktreeError::OntoHasNoBranch(TaskId(id))) if id == n));
            let mut review = brief(&fixture, "review", "Review");
            review.of = Some(TaskId(n));
            let result = prepare(&review, &board, &fixture.home);
            assert!(matches!(result, Err(WorktreeError::OfHasNoBranch(TaskId(id))) if id == n));
        }
        assert!(!expected_dir(&fixture).exists());
    }

    #[test]
    fn a_missing_task_is_a_board_error() {
        let fixture = Fixture::new("wt-missing");
        let board = FakeBoard::new(Vec::new());
        let mut stacked = brief(&fixture, "implement", "Stacked");
        stacked.base = Some(TaskId(3));
        let result = prepare(&stacked, &board, &fixture.home);
        assert!(matches!(result, Err(WorktreeError::Board(_))));
    }

    #[test]
    fn a_subdirectory_repo_names_the_project_by_its_last_component() {
        let fixture = Fixture::new("wt-subdir");
        let sub = fixture.repo.join("crates").join("store");
        fs::create_dir_all(&sub).unwrap();
        let board = FakeBoard::new(Vec::new());
        let mut brief = brief(&fixture, "implement", "Sub");
        brief.repo = sub;
        let checkout = prepare(&brief, &board, &fixture.home).unwrap();
        let expected = fixture.home.join("worktrees").join("store").join("7");
        assert_eq!(checkout.dir, expected);
        assert!(
            checkout.dir.join("README").exists(),
            "the whole repository is checked out"
        );
    }

    #[test]
    fn an_existing_directory_that_is_not_a_checkout_is_an_error() {
        let fixture = Fixture::new("wt-notcheckout");
        let dir = expected_dir(&fixture);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("keep.txt"), "mine\n").unwrap();
        let board = FakeBoard::new(Vec::new());
        let result = prepare(&brief(&fixture, "implement", "X"), &board, &fixture.home);
        assert!(matches!(result, Err(WorktreeError::NotACheckout(_))));
        assert_eq!(fs::read_to_string(dir.join("keep.txt")).unwrap(), "mine\n");
    }

    fn brief_for(fixture: &Fixture, id: u64, role: &str, text: &str) -> Brief {
        let mut brief = brief(fixture, role, text);
        brief.id = TaskId(id);
        brief
    }

    #[test]
    fn a_label_that_names_a_tag_or_head_is_not_a_branch() {
        let fixture = Fixture::new("wt-tag");
        git(&fixture.repo, &["tag", "v1"]);
        let board = FakeBoard::new(vec![task(3, Some("v1")), task(4, Some("HEAD"))]);
        for n in [3, 4] {
            let mut onto = brief(&fixture, "polish", "Onto");
            onto.onto = Some(TaskId(n));
            let result = prepare(&onto, &board, &fixture.home);
            assert!(matches!(result, Err(WorktreeError::OntoHasNoBranch(TaskId(id))) if id == n));
            let mut stacked = brief(&fixture, "implement", "Stacked");
            stacked.base = Some(TaskId(n));
            let result = prepare(&stacked, &board, &fixture.home);
            assert!(matches!(result, Err(WorktreeError::BaseHasNoBranch(TaskId(id))) if id == n));
            let mut review = brief(&fixture, "review", "Review");
            review.of = Some(TaskId(n));
            let result = prepare(&review, &board, &fixture.home);
            assert!(matches!(result, Err(WorktreeError::OfHasNoBranch(TaskId(id))) if id == n));
        }
        assert!(!expected_dir(&fixture).exists());
    }

    #[test]
    fn a_base_branch_that_shares_a_tag_name_is_cut_from_the_branch() {
        let fixture = Fixture::new("wt-tag-branch");
        git(&fixture.repo, &["tag", BASE_BRANCH]);
        let base_head = fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let mut stacked = brief(&fixture, "implement", "Stacked");
        stacked.base = Some(TaskId(3));
        let checkout = prepare(&stacked, &board, &fixture.home).unwrap();
        assert_eq!(checkout.start_head, base_head);
    }

    #[test]
    fn release_removes_a_clean_worktree_and_keeps_its_branch() {
        let fixture = Fixture::new("wt-release");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        let committed = fixture.commit(&checkout.dir, "work.txt", "work");
        release(&checkout, &fixture.repo, &fixture.home).unwrap();
        assert!(!checkout.dir.exists());
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "rider/7-work"]),
            committed
        );
    }

    #[test]
    fn release_refuses_a_dirty_worktree_and_leaves_it() {
        let fixture = Fixture::new("wt-release-dirty");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        fs::write(checkout.dir.join("scratch.txt"), "untracked\n").unwrap();
        let result = release(&checkout, &fixture.repo, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::Git(GitError::Exit { .. }))
        ));
        assert!(checkout.dir.join("scratch.txt").exists());
        fs::remove_file(checkout.dir.join("scratch.txt")).unwrap();
        fs::write(checkout.dir.join("README"), "edited\n").unwrap();
        let result = release(&checkout, &fixture.repo, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::Git(GitError::Exit { .. }))
        ));
        let readme = fs::read_to_string(checkout.dir.join("README")).unwrap();
        assert_eq!(readme, "edited\n");
    }

    #[test]
    fn release_refuses_the_main_checkout() {
        let fixture = Fixture::new("wt-release-main");
        let head = git(&fixture.repo, &["rev-parse", "HEAD"]);
        let outside = Checkout {
            dir: fixture.repo.clone(),
            branch: Some("main".to_string()),
            start_head: head.clone(),
        };
        let result = release(&outside, &fixture.repo, &fixture.home);
        assert!(matches!(result, Err(WorktreeError::NotReleasable { .. })));
        assert!(fixture.repo.join("README").exists());
        let inside = expected_dir(&fixture);
        fs::create_dir_all(&inside).unwrap();
        crate::git::testing::init_repo(&inside);
        let checkout = Checkout {
            dir: inside.clone(),
            branch: Some("main".to_string()),
            start_head: head,
        };
        let result = release(&checkout, &inside, &fixture.home);
        assert!(matches!(result, Err(WorktreeError::NotReleasable { .. })));
        assert!(inside.join("README").exists());
    }

    #[test]
    fn release_refuses_a_worktree_of_another_repository() {
        let fixture = Fixture::new("wt-release-other");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        let other = fixture.root.join("other");
        fs::create_dir_all(&other).unwrap();
        crate::git::testing::init_repo(&other);
        let result = release(&checkout, &other, &fixture.home);
        assert!(matches!(result, Err(WorktreeError::NotReleasable { .. })));
        assert!(checkout.dir.join("README").exists());
    }

    #[test]
    fn onto_branch_checked_out_elsewhere_is_busy_until_released() {
        let fixture = Fixture::new("wt-busy");
        let board = FakeBoard::new(vec![task(3, Some("rider/3-store"))]);
        let base = prepare(
            &brief_for(&fixture, 3, "implement", "Store"),
            &board,
            &fixture.home,
        );
        let base = base.unwrap();
        let committed = fixture.commit(&base.dir, "store.txt", "store");
        let mut polish = brief(&fixture, "polish", "Polish");
        polish.onto = Some(TaskId(3));
        let result = prepare(&polish, &board, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::OntoBranchBusy(TaskId(3)))
        ));
        assert!(!expected_dir(&fixture).exists());
        release(&base, &fixture.repo, &fixture.home).unwrap();
        let checkout = prepare(&polish, &board, &fixture.home).unwrap();
        assert_eq!(checkout.branch.as_deref(), Some("rider/3-store"));
        assert_eq!(checkout.start_head, committed);
        assert_eq!(
            current_branch(&checkout.dir).as_deref(),
            Some("rider/3-store")
        );
    }

    #[test]
    fn onto_branch_checked_out_in_the_main_checkout_is_busy() {
        let fixture = Fixture::new("wt-busy-main");
        let board = FakeBoard::new(vec![task(3, Some("main"))]);
        let mut polish = brief(&fixture, "polish", "Polish");
        polish.onto = Some(TaskId(3));
        let result = prepare(&polish, &board, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::OntoBranchBusy(TaskId(3)))
        ));
    }

    #[test]
    fn an_unrelated_repository_at_the_path_is_not_a_checkout() {
        let fixture = Fixture::new("wt-unrelated");
        let dir = expected_dir(&fixture);
        fs::create_dir_all(&dir).unwrap();
        crate::git::testing::init_repo(&dir);
        let board = FakeBoard::new(Vec::new());
        let result = prepare(&brief(&fixture, "implement", "X"), &board, &fixture.home);
        assert!(matches!(result, Err(WorktreeError::NotACheckout(_))));
        assert!(dir.join("README").exists());
    }

    fn released(fixture: &Fixture, checkout: &Checkout) -> Result<(), WorktreeError> {
        release(checkout, &fixture.repo, &fixture.home)
    }

    fn refused_because(result: Result<(), WorktreeError>, expected: &str) {
        match result {
            Err(WorktreeError::NotReleasable { reason, .. }) => assert_eq!(reason, expected),
            other => panic!("expected NotReleasable({expected}), got {other:?}"),
        }
    }

    #[test]
    fn a_branch_label_too_long_for_a_full_ref_is_no_branch() {
        let fixture = Fixture::new("wt-long");
        let long = format!("b/{}", "x".repeat(248));
        assert_eq!(long.len(), 250);
        fixture.branch_ahead(&long);
        let board = FakeBoard::new(vec![task(3, Some(&long))]);
        let mut stacked = brief(&fixture, "implement", "Stacked");
        stacked.base = Some(TaskId(3));
        let result = prepare(&stacked, &board, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::BaseHasNoBranch(TaskId(3)))
        ));
        let mut onto = brief(&fixture, "polish", "Onto");
        onto.onto = Some(TaskId(3));
        let result = prepare(&onto, &board, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::OntoHasNoBranch(TaskId(3)))
        ));
        let mut review = brief(&fixture, "review", "Review");
        review.of = Some(TaskId(3));
        let result = prepare(&review, &board, &fixture.home);
        assert!(matches!(
            result,
            Err(WorktreeError::OfHasNoBranch(TaskId(3)))
        ));
    }

    #[test]
    fn a_new_branch_checked_out_elsewhere_is_busy() {
        let fixture = Fixture::new("wt-branch-busy");
        fixture.branch_ahead("rider/7-work");
        let elsewhere = fixture.root.join("elsewhere");
        let path = elsewhere.to_str().unwrap();
        git(
            &fixture.repo,
            &["worktree", "add", "-q", "--", path, "rider/7-work"],
        );
        let board = FakeBoard::new(Vec::new());
        let result = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        match result {
            Err(WorktreeError::BranchBusy(branch)) => assert_eq!(branch, "rider/7-work"),
            other => panic!("expected BranchBusy, got {other:?}"),
        }
        assert!(!expected_dir(&fixture).exists());
    }

    #[test]
    fn release_refuses_a_worktree_moved_off_its_branch() {
        let fixture = Fixture::new("wt-release-moved");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        git(&checkout.dir, &["checkout", "-q", "--detach"]);
        let orphan = fixture.commit(&checkout.dir, "orphan.txt", "orphan");
        refused_because(released(&fixture, &checkout), "not on its branch");
        git(&checkout.dir, &["checkout", "-q", "-b", "other"]);
        refused_because(released(&fixture, &checkout), "not on its branch");
        assert_eq!(git(&checkout.dir, &["rev-parse", "HEAD"]), orphan);
    }

    #[test]
    fn release_refuses_a_detached_checkout_that_moved() {
        let fixture = Fixture::new("wt-release-detached");
        fixture.branch_ahead(BASE_BRANCH);
        let board = FakeBoard::new(vec![task(3, Some(BASE_BRANCH))]);
        let mut review = brief(&fixture, "review", "Review");
        review.of = Some(TaskId(3));
        let checkout = prepare(&review, &board, &fixture.home).unwrap();
        let moved = fixture.commit(&checkout.dir, "notes.txt", "notes");
        refused_because(released(&fixture, &checkout), "moved from its start");
        assert_eq!(git(&checkout.dir, &["rev-parse", "HEAD"]), moved);
        git(
            &checkout.dir,
            &["checkout", "-q", "--detach", &checkout.start_head],
        );
        released(&fixture, &checkout).unwrap();
        assert!(!checkout.dir.exists());
    }

    #[test]
    fn release_refuses_a_rebase_or_bisect_in_progress() {
        let fixture = Fixture::new("wt-release-busy");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        fixture.commit(&checkout.dir, "work.txt", "work");
        assert!(!git_succeeds(
            &checkout.dir,
            &["rebase", "-q", "--exec", "false", "HEAD~1"]
        ));
        refused_because(released(&fixture, &checkout), "an operation is in progress");
        git(&checkout.dir, &["rebase", "--abort"]);
        git(&checkout.dir, &["bisect", "start"]);
        refused_because(released(&fixture, &checkout), "an operation is in progress");
        git(&checkout.dir, &["bisect", "reset"]);
        released(&fixture, &checkout).unwrap();
    }

    #[test]
    fn release_removes_a_worktree_holding_ignored_build_output_and_keeps_its_branch() {
        let fixture = Fixture::new("wt-release-ignored");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        let committed = fixture.commit(&checkout.dir, ".gitignore", "target/\n");
        fs::create_dir_all(checkout.dir.join("target")).unwrap();
        fs::write(checkout.dir.join("target").join("build.log"), "output\n").unwrap();
        released(&fixture, &checkout).unwrap();
        assert!(!checkout.dir.exists());
        assert_eq!(
            git(&fixture.repo, &["rev-parse", "rider/7-work"]),
            committed
        );
    }

    #[test]
    fn release_resolves_a_symlinked_path_before_it_acts() {
        let fixture = Fixture::new("wt-release-link");
        let board = FakeBoard::new(Vec::new());
        let checkout = prepare(&brief(&fixture, "implement", "Work"), &board, &fixture.home);
        let checkout = checkout.unwrap();
        let link = fixture.home.join("worktrees").join("link");
        std::os::unix::fs::symlink(&checkout.dir, &link).unwrap();
        let through_link = Checkout {
            dir: link.clone(),
            ..checkout.clone()
        };
        released(&fixture, &through_link).unwrap();
        assert!(!checkout.dir.exists());
        assert_eq!(
            git(&fixture.repo, &["worktree", "list", "--porcelain"])
                .lines()
                .count(),
            3
        );
    }
}
