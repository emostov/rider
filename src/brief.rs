//! A task's brief: its labels and text, checked and turned into what an agent run needs.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::board::{LABELS_MAX, TaskId, TaskView};
use crate::git;
use crate::roles::{RoleSpec, role_from_label};

/// Upper bound, in characters, on a label value echoed back in an error message.
pub const ECHO_CHARS_MAX: usize = 80;
/// Upper bound, in bytes, on the `repo` label's path.
pub const REPO_BYTES_MAX: usize = 4096;
/// Upper bound, in bytes, on the `from` label's branch name.
pub const FROM_BYTES_MAX: usize = 255;
/// The longest decimal ID that fits a `u64`.
const ID_DIGITS_MAX: usize = 20;

/// A task's labels and text, checked: everything needed to run one agent on it.
#[derive(Debug, Clone)]
pub struct Brief {
    pub id: TaskId,
    pub role: &'static RoleSpec,
    /// Absolute, and the root or inside of a git repository when parsed.
    pub repo: PathBuf,
    pub base: Option<TaskId>,
    pub onto: Option<TaskId>,
    /// Required when the role does not expect commits.
    pub of: Option<TaskId>,
    /// Label `from`: the branch to cut from when `base` is absent.
    pub from: Option<String>,
    /// Label `resume=1`: continue the agent session of the `onto` task where one is recorded.
    pub resume: bool,
    /// Label `restack=1`: the task rebases its `onto` branch, so it may drop the starting commit.
    pub restack: bool,
    pub github: bool,
    pub untrusted: bool,
    pub text: String,
}

/// Why a brief is invalid. Values echoed from labels are cut to `ECHO_CHARS_MAX` characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefError {
    NoRole,
    UnknownRole(String),
    RoleOff(String),
    NoRepo,
    RepoNotAbsolute(String),
    /// The repo path, or the reason it could not be checked.
    RepoNotAGitRepo(String),
    BaseAndOnto,
    BadTaskId {
        label: &'static str,
        value: String,
    },
    ReviewWithoutOf,
    BadFrom(String),
    BadResume(String),
    BadRestack(String),
    BadSource(String),
    BadUntrusted(String),
    UntrustedInGit(&'static str),
    RestackWithoutOnto,
    /// An `onto` task on a role without commits, which names only a session to resume.
    OntoWithoutResume {
        onto: TaskId,
        role: &'static str,
    },
    /// The `base` task, which the task does not need.
    BaseNotNeeded(TaskId),
    OfSelf,
    OfCanceled(TaskId),
    OfCycle(TaskId),
    /// An unfinished task on the branch of the `onto` task that the task does not need.
    OntoNotNeeded {
        unfinished: TaskId,
        onto: TaskId,
    },
    /// `unchecked` unfinished tasks, or their `onto` tasks, lie past rider's bounds, so the
    /// unfinished tasks on the branch of the `onto` task are not all known.
    OntoUnchecked {
        onto: TaskId,
        unchecked: usize,
    },
}

impl fmt::Display for BriefError {
    /// One sentence telling the lead which label to change.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BriefError::NoRole => write!(
                f,
                "Set the `role` label to one of implement, light, review, native-review, \
                 security or qa."
            ),
            BriefError::UnknownRole(value) => write!(
                f,
                "Change the `role` label from {value:?} to one of implement, light, \
                 review, native-review, security or qa."
            ),
            BriefError::RoleOff(role) => write!(
                f,
                "Change the `role` label from {role:?}: the role is turned off by \
                 `enabled = false` under [roles.{role}] in rider.toml."
            ),
            BriefError::NoRepo => write!(
                f,
                "Set the `repo` label to the absolute path of the repository."
            ),
            BriefError::RepoNotAbsolute(value) => write!(
                f,
                "Change the `repo` label from {value:?} to an absolute path."
            ),
            BriefError::RepoNotAGitRepo(reason) => write!(
                f,
                "Change the `repo` label to the path of a git repository: {reason}."
            ),
            BriefError::BaseAndOnto => write!(
                f,
                "Remove either the `base` label or the `onto` label, since a task may \
                 not have both."
            ),
            BriefError::BadTaskId { label, value } => write!(
                f,
                "Change the `{label}` label from {value:?} to a task id such as #812."
            ),
            BriefError::ReviewWithoutOf => write!(
                f,
                "Set the `of` label to the id of the task whose work this role checks."
            ),
            BriefError::BadFrom(value) => write!(
                f,
                "Change the `from` label from {value:?} to a branch name without \
                 spaces or `..` that does not start with `-`."
            ),
            BriefError::BadResume(value) => write!(
                f,
                "Change the `resume` label from {value:?} to 1, or remove it."
            ),
            BriefError::BadRestack(value) => write!(
                f,
                "Change the `restack` label from {value:?} to 1, or remove it."
            ),
            BriefError::BadSource(value) => write!(
                f,
                "Change the `source` label from {value:?} to github, or remove it."
            ),
            BriefError::BadUntrusted(value) => write!(
                f,
                "Change the `untrusted` label from {value:?} to 1, or remove it."
            ),
            BriefError::UntrustedInGit(key) => write!(
                f,
                "Remove the `{key}` label: an untrusted task gets no git worktree or branch, \
                 only a scratch directory."
            ),
            BriefError::RestackWithoutOnto => write!(
                f,
                "Remove the `restack` label, or set `onto` to the task whose branch this task \
                 rebases."
            ),
            BriefError::OntoWithoutResume { onto, role } => write!(
                f,
                "Set the `resume` label to 1 to continue the agent session of task #{}, or \
                 remove the `onto` label: a {role} task works on no branch.",
                onto.0
            ),
            BriefError::BaseNotNeeded(base) => write!(
                f,
                "Add task #{} to this task's needs, or remove the `base` label: a task is cut \
                 from its `base` task's branch only once that task is done.",
                base.0
            ),
            BriefError::OfSelf => write!(
                f,
                "Change the `of` label to another task: a task cannot check its own work."
            ),
            BriefError::OfCanceled(of) => write!(
                f,
                "Change the `of` label: task #{} was canceled, so it is never done.",
                of.0
            ),
            BriefError::OfCycle(of) => write!(
                f,
                "Change the `of` label of this task or of task #{}: the `of` labels form a \
                 cycle, so none of these tasks is ever done first.",
                of.0
            ),
            BriefError::OntoNotNeeded { unfinished, onto } => write!(
                f,
                "Add task #{} to this task's needs: it is unfinished on the branch of task \
                 #{}, which this task works `onto`.",
                unfinished.0, onto.0
            ),
            BriefError::OntoUnchecked { onto, unchecked } => write!(
                f,
                "Finish or cancel unfinished tasks, or remove the `onto` label: {unchecked} \
                 of them are past rider's bounds, so it cannot check which are unfinished on \
                 the branch of task #{}.",
                onto.0
            ),
        }
    }
}

/// The brief of a task read from the board, or the first label the lead must fix.
/// Checks that only read labels come first; the git check spawns `git` and comes last.
pub fn parse_brief(view: &TaskView) -> Result<Brief, BriefError> {
    assert!(
        view.labels.len() <= LABELS_MAX,
        "the board bounds the labels"
    );
    let role = label(view, "role").ok_or(BriefError::NoRole)?;
    let role = role_from_label(role).ok_or_else(|| BriefError::UnknownRole(echo(role)))?;
    if !role.enabled {
        return Err(BriefError::RoleOff(role.label.to_string()));
    }
    let repo = parse_repo(label(view, "repo"))?;
    let base = parse_id_label(view, "base")?;
    let onto = parse_id_label(view, "onto")?;
    let of = parse_id_label(view, "of")?;
    if base.is_some() && onto.is_some() {
        return Err(BriefError::BaseAndOnto);
    }
    let untrusted = match label(view, "untrusted") {
        None | Some("0") => false,
        Some("1") => true,
        Some(other) => return Err(BriefError::BadUntrusted(echo(other))),
    };
    if role.needs_of && of.is_none() && !untrusted {
        return Err(BriefError::ReviewWithoutOf);
    }
    if of == Some(view.id) {
        return Err(BriefError::OfSelf);
    }
    if let Some(base) = base.filter(|base| !view.needs.contains(base)) {
        return Err(BriefError::BaseNotNeeded(base));
    }
    let from = label(view, "from").map(parse_from).transpose()?;
    let resume = match label(view, "resume") {
        None | Some("0") => false,
        Some("1") => true,
        Some(other) => return Err(BriefError::BadResume(echo(other))),
    };
    let restack = match label(view, "restack") {
        None | Some("0") => false,
        Some("1") => true,
        Some(other) => return Err(BriefError::BadRestack(echo(other))),
    };
    let github = match label(view, "source") {
        None => false,
        Some("github") => true,
        Some(other) => return Err(BriefError::BadSource(echo(other))),
    };
    if restack && onto.is_none() {
        return Err(BriefError::RestackWithoutOnto);
    }
    if let Some(onto) = onto.filter(|_| !role.commits_expected && !resume) {
        let role = role.label;
        return Err(BriefError::OntoWithoutResume { onto, role });
    }
    let in_git = [
        ("base", base.is_some()),
        ("from", from.is_some()),
        ("restack", restack),
        ("onto", onto.is_some() && role.commits_expected),
    ];
    if let Some((key, _)) = in_git.iter().find(|(_, set)| untrusted && *set) {
        return Err(BriefError::UntrustedInGit(key));
    }
    check_git_repo(&repo)?;
    let brief = Brief {
        id: view.id,
        role,
        repo,
        base,
        onto,
        of,
        from,
        resume,
        restack,
        github,
        untrusted,
        text: view.text.clone(),
    };
    assert!(brief.repo.is_absolute());
    assert!(!brief.restack || brief.onto.is_some());
    assert!(brief.role.commits_expected || brief.onto.is_none() || brief.resume);
    assert!(brief.base.is_none_or(|base| view.needs.contains(&base)));
    assert!(!(brief.base.is_some() && brief.onto.is_some()));
    assert!(!brief.role.needs_of || brief.of.is_some() || brief.untrusted);
    assert!(!brief.untrusted || (brief.base.is_none() && brief.from.is_none()));
    assert!(brief.of != Some(brief.id));
    assert!(brief.role.commits_expected || brief.role.needs_of);
    Ok(brief)
}

/// The value of the last label with this key, or `None` when it is absent or empty.
pub(crate) fn label<'a>(view: &'a TaskView, key: &str) -> Option<&'a str> {
    assert!(!key.is_empty());
    assert!(view.labels.len() <= LABELS_MAX);
    let mut found: Option<&'a str> = None;
    for (label_key, value) in view.labels.iter().take(LABELS_MAX) {
        if label_key == key {
            found = Some(value.as_str());
        }
    }
    found.filter(|value| !value.is_empty())
}

/// A label value fit to quote in an error message: at most `ECHO_CHARS_MAX` characters,
/// with `…` marking a cut.
pub(crate) fn echo(value: &str) -> String {
    let mut echoed: String = value.chars().take(ECHO_CHARS_MAX).collect();
    if echoed.len() < value.len() {
        echoed.push('…');
    }
    assert!(echoed.chars().count() <= ECHO_CHARS_MAX + 1);
    assert!(value.is_empty() || !echoed.is_empty());
    echoed
}

fn parse_repo(value: Option<&str>) -> Result<PathBuf, BriefError> {
    let value = value.ok_or(BriefError::NoRepo)?;
    assert!(!value.is_empty(), "empty labels are absent");
    let repo = PathBuf::from(value);
    if !repo.is_absolute() {
        return Err(BriefError::RepoNotAbsolute(echo(value)));
    }
    if value.len() > REPO_BYTES_MAX {
        let reason = format!("the path is longer than {REPO_BYTES_MAX} bytes");
        return Err(BriefError::RepoNotAGitRepo(reason));
    }
    assert!(repo.is_absolute());
    Ok(repo)
}

/// Succeeds when `repo` is a directory and `git -C <repo> rev-parse --git-dir` succeeds.
fn check_git_repo(repo: &Path) -> Result<(), BriefError> {
    assert!(repo.is_absolute());
    assert!(repo.as_os_str().len() <= REPO_BYTES_MAX);
    let shown = echo(&repo.to_string_lossy());
    if !fs::metadata(repo).is_ok_and(|metadata| metadata.is_dir()) {
        let reason = format!("{shown:?} is not a directory");
        return Err(BriefError::RepoNotAGitRepo(reason));
    }
    let status = git::hardened()
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => {
            let reason = format!("{shown:?} is not inside a git repository");
            Err(BriefError::RepoNotAGitRepo(reason))
        }
        Err(error) => {
            let reason = format!("git could not be started to check {shown:?} ({error})");
            Err(BriefError::RepoNotAGitRepo(reason))
        }
    }
}

/// The task id in the label with this key: decimal, with an optional leading `#`.
pub(crate) fn parse_id_label(
    view: &TaskView,
    key: &'static str,
) -> Result<Option<TaskId>, BriefError> {
    assert!(key == "base" || key == "onto" || key == "of");
    let Some(value) = label(view, key) else {
        return Ok(None);
    };
    let bad = || BriefError::BadTaskId {
        label: key,
        value: echo(value),
    };
    let digits = value.strip_prefix('#').unwrap_or(value);
    let canonical = !digits.is_empty()
        && digits.len() <= ID_DIGITS_MAX
        && digits.bytes().all(|b| b.is_ascii_digit());
    if !canonical {
        return Err(bad());
    }
    let number = digits.parse::<u64>().map_err(|_| bad())?;
    assert!(number.to_string().len() <= digits.len());
    Ok(Some(TaskId(number)))
}

/// The branch name in the `from` label.
fn parse_from(value: &str) -> Result<String, BriefError> {
    assert!(!value.is_empty(), "empty labels are absent");
    let valid = !value.trim().is_empty()
        && value.len() <= FROM_BYTES_MAX
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.chars().any(char::is_whitespace);
    if !valid {
        return Err(BriefError::BadFrom(echo(value)));
    }
    assert!(!value.starts_with('-') && !value.contains(char::is_whitespace));
    Ok(value.to_string())
}
