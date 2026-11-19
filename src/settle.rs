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

#[cfg(test)]
mod tests {
    use super::*;

    fn done() -> Outcome<'static> {
        Outcome {
            exit: Exit::Code(0),
            state_after: TaskState::Claimed,
            stream_failed: false,
            clean: true,
            branch: Some("rider/7-token-store"),
            on_branch: true,
            head_moved: true,
            keeps_start: true,
            own_commits: 1,
            commits_expected: true,
            last_message: Some("Extracted the token store."),
        }
    }

    fn fail(reason: &str) -> Verdict {
        Verdict::Fail {
            reason: reason.to_string(),
        }
    }

    #[test]
    fn a_blocked_task_is_left_blocked() {
        let outcome = Outcome {
            state_after: TaskState::Blocked,
            ..done()
        };
        assert_eq!(judge(&outcome), Verdict::LeaveBlocked);
    }

    #[test]
    fn a_timed_out_attempt_fails() {
        let outcome = Outcome {
            exit: Exit::TimedOut,
            ..done()
        };
        assert_eq!(judge(&outcome), fail("timed out after 55 minutes"));
    }

    #[test]
    fn a_non_zero_exit_or_a_signal_fails() {
        let outcome = Outcome {
            exit: Exit::Code(2),
            ..done()
        };
        assert_eq!(judge(&outcome), fail("agent exited 2"));
        let outcome = Outcome {
            exit: Exit::Signal,
            ..done()
        };
        assert_eq!(judge(&outcome), fail("agent exited on a signal"));
    }

    #[test]
    fn a_stream_that_reports_failure_fails_even_on_exit_zero() {
        let outcome = Outcome {
            stream_failed: true,
            ..done()
        };
        assert_eq!(
            judge(&outcome),
            fail("agent reported failure in its output stream")
        );
    }

    #[test]
    fn dirty_tree_fails_even_on_exit_zero() {
        let outcome = Outcome {
            clean: false,
            ..done()
        };
        let expected = fail("no committed work: the worktree has uncommitted changes");
        assert_eq!(judge(&outcome), expected);
        let review = Outcome {
            clean: false,
            branch: None,
            commits_expected: false,
            head_moved: false,
            ..done()
        };
        assert_eq!(judge(&review), expected);
    }

    #[test]
    fn a_role_that_must_commit_fails_without_commits_of_its_own() {
        let expected = fail("no committed work: the branch has no commits of its own");
        let moved = Outcome {
            own_commits: 0,
            ..done()
        };
        assert_eq!(
            judge(&moved),
            expected,
            "a branch moved onto others' commits"
        );
        let still = Outcome {
            head_moved: false,
            own_commits: 0,
            ..done()
        };
        assert_eq!(judge(&still), expected);
    }

    #[test]
    fn a_worktree_moved_off_its_branch_fails() {
        let outcome = Outcome {
            on_branch: false,
            ..done()
        };
        let reason = "no committed work: the worktree is not on its branch rider/7-token-store";
        assert_eq!(judge(&outcome), fail(reason));
    }

    #[test]
    fn a_role_that_must_commit_fails_on_a_detached_checkout() {
        let outcome = Outcome {
            branch: None,
            ..done()
        };
        let reason = "no committed work: the worktree is not on a branch";
        assert_eq!(judge(&outcome), fail(reason));
    }

    #[test]
    fn a_review_that_moved_or_attached_its_checkout_fails() {
        let moved = Outcome {
            branch: None,
            commits_expected: false,
            ..done()
        };
        assert_eq!(judge(&moved), fail("review changed the checkout"));
        let attached = Outcome {
            branch: None,
            commits_expected: false,
            head_moved: false,
            on_branch: false,
            ..done()
        };
        assert_eq!(judge(&attached), fail("review changed the checkout"));
    }

    #[test]
    fn a_branch_that_lost_its_starting_commit_fails() {
        let outcome = Outcome {
            keeps_start: false,
            ..done()
        };
        let reason = "no committed work: the branch no longer contains its starting commit";
        assert_eq!(judge(&outcome), fail(reason));
    }

    #[test]
    fn a_clean_attempt_that_committed_completes_with_the_last_message() {
        let expected = Verdict::Complete {
            result: "Extracted the token store.".to_string(),
        };
        assert_eq!(judge(&done()), expected);
    }

    #[test]
    fn a_completion_without_a_last_message_says_completed() {
        let completed = Verdict::Complete {
            result: "completed".to_string(),
        };
        let silent = Outcome {
            last_message: None,
            ..done()
        };
        assert_eq!(judge(&silent), completed);
        let blank = Outcome {
            last_message: Some(" \n "),
            ..done()
        };
        assert_eq!(judge(&blank), completed);
    }

    #[test]
    fn review_role_completes_without_commits() {
        let outcome = Outcome {
            branch: None,
            head_moved: false,
            own_commits: 0,
            commits_expected: false,
            last_message: Some("Two findings, one blocking."),
            ..done()
        };
        let expected = Verdict::Complete {
            result: "Two findings, one blocking.".to_string(),
        };
        assert_eq!(judge(&outcome), expected);
    }

    #[test]
    fn blocked_task_is_left_alone_whatever_the_exit() {
        let exits = [Exit::Code(0), Exit::Code(1), Exit::Signal, Exit::TimedOut];
        for exit in exits {
            let outcome = Outcome {
                exit,
                state_after: TaskState::Blocked,
                stream_failed: true,
                clean: false,
                head_moved: false,
                ..done()
            };
            assert_eq!(judge(&outcome), Verdict::LeaveBlocked, "{exit:?}");
        }
    }
}
