//! The judgment of one finished attempt, from the evidence rather than the agent's word.

use crate::board::TaskState;
use crate::harness::{ATTEMPT_SECONDS_MAX, Exit};

const RESULT_FALLBACK: &str = "completed";

/// What to do with a task after one attempt at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Complete {
        result: String,
    },
    Fail {
        reason: String,
    },
    /// The task is blocked, by its worker or by rider, and stays as it is.
    LeaveBlocked,
}

/// The evidence one attempt left behind.
pub struct Outcome<'a> {
    pub exit: Exit,
    pub state_after: TaskState,
    /// The agent's output stream records a failed run, whatever its exit status.
    pub stream_failed: bool,
    pub clean: bool,
    /// The task's branch, or `None` when its checkout is detached.
    pub branch: Option<&'a str>,
    /// HEAD is still on `branch`, or still detached when `branch` is `None`.
    pub on_branch: bool,
    /// HEAD points somewhere other than the starting commit; it judges a detached checkout.
    pub head_moved: bool,
    /// The branch still contains the starting commit, or the task may rewrite it.
    pub keeps_start: bool,
    /// The commits only the task's branch holds, beyond where the task began.
    pub own_commits: u64,
    pub commits_expected: bool,
    pub last_message: Option<&'a str>,
}

/// The verdict on one attempt; the first rule that applies wins.
pub fn judge(o: &Outcome) -> Verdict {
    let fail = |reason: String| Verdict::Fail { reason };
    let verdict = if o.state_after == TaskState::Blocked {
        Verdict::LeaveBlocked
    } else if o.exit == Exit::TimedOut {
        fail(format!(
            "timed out after {} minutes",
            ATTEMPT_SECONDS_MAX / 60
        ))
    } else if o.exit == Exit::Signal {
        fail("agent exited on a signal".to_string())
    } else if let Exit::Code(code) = o.exit
        && code != 0
    {
        fail(format!("agent exited {code}"))
    } else if o.stream_failed {
        fail("agent reported failure in its output stream".to_string())
    } else if let Some(branch) = o.branch
        && !o.on_branch
    {
        fail(format!(
            "no committed work: the worktree is not on its branch {branch}"
        ))
    } else if o.branch.is_none() && o.commits_expected {
        fail("no committed work: the worktree is not on a branch".to_string())
    } else if o.branch.is_none() && (!o.on_branch || o.head_moved) {
        fail("review changed the checkout".to_string())
    } else if !o.clean {
        fail("no committed work: the worktree has uncommitted changes".to_string())
    } else if o.branch.is_some() && !o.keeps_start {
        fail("no committed work: the branch no longer contains its starting commit".to_string())
    } else if o.commits_expected && o.own_commits == 0 {
        fail("no committed work: the branch has no commits of its own".to_string())
    } else {
        let message = o.last_message.map(str::trim).filter(|m| !m.is_empty());
        let result = message.unwrap_or(RESULT_FALLBACK).to_string();
        Verdict::Complete { result }
    };
    let text = match &verdict {
        Verdict::Complete { result } => Some(result),
        Verdict::Fail { reason } => Some(reason),
        Verdict::LeaveBlocked => None,
    };
    assert!(text.is_none_or(|text| !text.trim().is_empty()));
    assert!(o.state_after != TaskState::Blocked || verdict == Verdict::LeaveBlocked);
    verdict
}
