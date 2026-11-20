//! A task's brief: its labels and text, checked and turned into what an agent run needs.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

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
    pub text: String,
}

/// Why a brief is invalid. Values echoed from labels are cut to `ECHO_CHARS_MAX` characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefError {
    NoRole,
    UnknownRole(String),
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
    RestackWithoutOnto,
    /// The `base` task, which the task does not need.
    BaseNotNeeded(TaskId),
    /// The `of` task, which the task does not need.
    OfNotNeeded(TaskId),
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
                "Set the `role` label to one of implement, light, review, polish, security or qa."
            ),
            BriefError::UnknownRole(value) => write!(
                f,
                "Change the `role` label from {value:?} to one of implement, light, \
                 review, polish, security or qa."
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
            BriefError::RestackWithoutOnto => write!(
                f,
                "Remove the `restack` label, or set `onto` to the task whose branch this task \
                 rebases."
            ),
            BriefError::BaseNotNeeded(base) => write!(
                f,
                "Add task #{} to this task's needs, or remove the `base` label: a task is cut \
                 from its `base` task's branch only once that task is done.",
                base.0
            ),
            BriefError::OfNotNeeded(of) => write!(
                f,
                "Add task #{} to this task's needs, or change the `of` label: a task checks \
                 the work of its `of` task only once that task is done.",
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
    let repo = parse_repo(label(view, "repo"))?;
    let base = parse_id_label(view, "base")?;
    let onto = parse_id_label(view, "onto")?;
    let of = parse_id_label(view, "of")?;
    if base.is_some() && onto.is_some() {
        return Err(BriefError::BaseAndOnto);
    }
    if role.needs_of && of.is_none() {
        return Err(BriefError::ReviewWithoutOf);
    }
    if let Some(base) = base.filter(|base| !view.needs.contains(base)) {
        return Err(BriefError::BaseNotNeeded(base));
    }
    if let Some(of) = of.filter(|of| role.needs_of && !view.needs.contains(of)) {
        return Err(BriefError::OfNotNeeded(of));
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
    if restack && onto.is_none() {
        return Err(BriefError::RestackWithoutOnto);
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
        text: view.text.clone(),
    };
    assert!(brief.repo.is_absolute());
    assert!(!brief.restack || brief.onto.is_some());
    assert!(brief.base.is_none_or(|base| view.needs.contains(&base)));
    assert!(!(brief.base.is_some() && brief.onto.is_some()));
    assert!(!brief.role.needs_of || brief.of.is_some());
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
    let mut git = Process::new("git");
    for name in git::ENV_REMOVED {
        git.env_remove(name);
    }
    let status = git
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::TaskState;
    use crate::feed::Cursor;
    use crate::roles::ROLES;
    use tempfile::TempDir;

    /// A fresh directory under the system temp dir, a git repository when `git` is set.
    fn scratch(git: bool) -> TempDir {
        let dir = TempDir::new().unwrap();
        if git {
            let status = Process::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .stdin(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }
        dir
    }

    fn view(labels: &[(&str, &str)]) -> TaskView {
        TaskView {
            id: TaskId(7),
            text: "Do the thing.".to_string(),
            state: TaskState::Ready,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            needs: Vec::new(),
            epoch: 0,
            claim: None,
            version: Cursor(0),
        }
    }

    /// An absolute path that does not exist, for checks that come before the git check.
    const NOWHERE: &str = "/nonexistent/rider-brief-repo";

    #[test]
    fn accepts_a_valid_implement_brief() {
        let repo = scratch(true);
        let mut task = view(&[
            ("role", "implement"),
            ("repo", repo.path().to_str().unwrap()),
            ("base", "12"),
        ]);
        task.needs.push(TaskId(12));
        let brief = parse_brief(&task).unwrap();
        assert_eq!(brief.id, TaskId(7));
        assert_eq!(brief.role.label, "implement");
        assert_eq!(brief.repo, repo.path());
        assert_eq!(brief.base, Some(TaskId(12)));
        assert_eq!(brief.onto, None);
        assert_eq!(brief.of, None);
        assert_eq!(brief.from, None);
        assert_eq!(brief.text, "Do the thing.");
    }

    #[test]
    fn accepts_a_review_with_hash_ids_and_from() {
        let repo = scratch(true);
        let mut task = view(&[
            ("role", "review"),
            ("repo", repo.path().to_str().unwrap()),
            ("onto", "#30"),
            ("of", "#812"),
            ("from", "main"),
            ("base", ""),
            ("restack", "1"),
        ]);
        task.needs.push(TaskId(812));
        let brief = parse_brief(&task).unwrap();
        assert!(brief.restack);
        assert_eq!(brief.role.label, "review");
        assert_eq!(brief.base, None);
        assert_eq!(brief.onto, Some(TaskId(30)));
        assert_eq!(brief.of, Some(TaskId(812)));
        assert_eq!(brief.from.as_deref(), Some("main"));
    }

    #[test]
    fn missing_role_is_no_role() {
        let error = parse_brief(&view(&[("repo", NOWHERE)])).unwrap_err();
        assert_eq!(error, BriefError::NoRole);
        let error = parse_brief(&view(&[("role", ""), ("repo", NOWHERE)])).unwrap_err();
        assert_eq!(error, BriefError::NoRole);
    }

    #[test]
    fn role_lead_is_unknown_role() {
        let error = parse_brief(&view(&[("role", "lead"), ("repo", NOWHERE)])).unwrap_err();
        assert_eq!(error, BriefError::UnknownRole("lead".to_string()));
    }

    #[test]
    fn role_messages_name_every_role() {
        assert_eq!(
            BriefError::NoRole.to_string(),
            "Set the `role` label to one of implement, light, review, polish, security or qa."
        );
        assert_eq!(
            BriefError::UnknownRole("lead".to_string()).to_string(),
            "Change the `role` label from \"lead\" to one of implement, light, review, polish, \
             security or qa."
        );
        let message = BriefError::NoRole.to_string();
        for role in ROLES.iter() {
            assert!(message.contains(role.label), "{} is missing", role.label);
        }
    }

    #[test]
    fn missing_repo_is_no_repo() {
        let error = parse_brief(&view(&[("role", "implement")])).unwrap_err();
        assert_eq!(error, BriefError::NoRepo);
        let error = parse_brief(&view(&[("role", "implement"), ("repo", "")])).unwrap_err();
        assert_eq!(error, BriefError::NoRepo);
    }

    #[test]
    fn relative_repo_is_rejected() {
        let error =
            parse_brief(&view(&[("role", "implement"), ("repo", "src/rider")])).unwrap_err();
        assert_eq!(error, BriefError::RepoNotAbsolute("src/rider".to_string()));
    }

    #[test]
    fn repo_without_git_is_rejected() {
        let plain = scratch(false);
        let error = parse_brief(&view(&[
            ("role", "implement"),
            ("repo", plain.path().to_str().unwrap()),
        ]));
        assert!(matches!(error, Err(BriefError::RepoNotAGitRepo(_))));
        let error = parse_brief(&view(&[("role", "implement"), ("repo", NOWHERE)]));
        assert!(matches!(error, Err(BriefError::RepoNotAGitRepo(_))));
    }

    #[test]
    fn base_and_onto_together_are_rejected() {
        let labels = [
            ("role", "implement"),
            ("repo", NOWHERE),
            ("base", "1"),
            ("onto", "2"),
        ];
        assert_eq!(
            parse_brief(&view(&labels)).unwrap_err(),
            BriefError::BaseAndOnto
        );
    }

    #[test]
    fn base_that_is_not_a_number_is_bad_task_id() {
        let bad = [
            "twelve",
            "-1",
            "#",
            "12a",
            "1 2",
            "99999999999999999999999",
            "##1",
        ];
        for value in bad {
            let labels = [("role", "implement"), ("repo", NOWHERE), ("base", value)];
            let error = parse_brief(&view(&labels)).unwrap_err();
            let expected = BriefError::BadTaskId {
                label: "base",
                value: value.to_string(),
            };
            assert_eq!(error, expected);
        }
        let labels = [("role", "review"), ("repo", NOWHERE), ("of", "x")];
        let error = parse_brief(&view(&labels)).unwrap_err();
        assert!(matches!(error, BriefError::BadTaskId { label: "of", .. }));
    }

    #[test]
    fn review_without_of_is_rejected() {
        for role in ["review", "security"] {
            let labels = [("role", role), ("repo", NOWHERE), ("base", "3")];
            assert_eq!(
                parse_brief(&view(&labels)).unwrap_err(),
                BriefError::ReviewWithoutOf
            );
        }
    }

    #[test]
    fn restack_is_one_and_needs_onto() {
        let labels = [("role", "implement"), ("repo", NOWHERE), ("restack", "yes")];
        let error = parse_brief(&view(&labels)).unwrap_err();
        assert_eq!(error, BriefError::BadRestack("yes".to_string()));
        let labels = [("role", "implement"), ("repo", NOWHERE), ("restack", "1")];
        let error = parse_brief(&view(&labels)).unwrap_err();
        assert_eq!(error, BriefError::RestackWithoutOnto);
    }

    #[test]
    fn bad_from_is_rejected() {
        let bad = ["", " ", "-main", "a b", "main..x", "tab\there", "a\nb"];
        for value in bad {
            let labels = [("role", "implement"), ("repo", NOWHERE), ("from", value)];
            let result = parse_brief(&view(&labels));
            if value.is_empty() {
                // An empty label is an absent label; the repo check then fails.
                assert!(matches!(result, Err(BriefError::RepoNotAGitRepo(_))));
            } else {
                assert!(matches!(result, Err(BriefError::BadFrom(_))), "{value:?}");
            }
        }
        assert!(
            BriefError::BadFrom("-x".into())
                .to_string()
                .contains("from")
        );
    }

    #[test]
    fn last_duplicate_label_wins() {
        let labels = [("role", "lead"), ("role", "implement"), ("repo", "rel")];
        let error = parse_brief(&view(&labels)).unwrap_err();
        assert_eq!(error, BriefError::RepoNotAbsolute("rel".to_string()));
    }

    #[test]
    fn every_error_message_names_the_label_to_fix() {
        assert!(BriefError::NoRepo.to_string().contains("repo"));
        assert!(
            BriefError::BaseAndOnto.to_string().contains("base")
                && BriefError::BaseAndOnto.to_string().contains("onto")
        );
        let cases = [
            (BriefError::NoRole, "role"),
            (BriefError::UnknownRole("lead".into()), "role"),
            (BriefError::NoRepo, "repo"),
            (BriefError::RepoNotAbsolute("x".into()), "repo"),
            (BriefError::RepoNotAGitRepo("x".into()), "repo"),
            (BriefError::BaseAndOnto, "base"),
            (
                BriefError::BadTaskId {
                    label: "onto",
                    value: "x".into(),
                },
                "onto",
            ),
            (BriefError::ReviewWithoutOf, "of"),
            (BriefError::BadFrom("x".into()), "from"),
            (BriefError::BadResume("x".into()), "resume"),
            (BriefError::BadRestack("x".into()), "restack"),
            (BriefError::RestackWithoutOnto, "restack"),
            (BriefError::BaseNotNeeded(TaskId(3)), "base"),
            (BriefError::OfNotNeeded(TaskId(3)), "of"),
            (
                BriefError::OntoNotNeeded {
                    unfinished: TaskId(4),
                    onto: TaskId(3),
                },
                "onto",
            ),
            (
                BriefError::OntoUnchecked {
                    onto: TaskId(3),
                    unchecked: 2,
                },
                "onto",
            ),
        ];
        for (error, label) in cases {
            let message = error.to_string();
            assert!(message.contains(&format!("`{label}`")), "{message}");
            assert!(message.ends_with('.'), "{message}");
        }
    }

    #[test]
    fn long_values_are_cut_in_messages() {
        let long = "x".repeat(ECHO_CHARS_MAX * 4);
        let error = parse_brief(&view(&[("role", &long), ("repo", NOWHERE)])).unwrap_err();
        let BriefError::UnknownRole(echo) = &error else {
            panic!("{error:?}");
        };
        assert!(echo.chars().count() <= ECHO_CHARS_MAX + 1);
    }
}
