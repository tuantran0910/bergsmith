//! Task and attempt lifecycle states and the execution retry budget.
//!
//! Implements `.private/BERGSMITH_PROJECT_PLAN.md` §2, "Task and attempt lifecycle" and
//! "Failure and retry dispositions".

use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum execution attempts per task, including the original attempt.
pub const MAX_EXECUTION_ATTEMPTS: u32 = 3;

/// Returns whether a task that has started `attempts_started` attempts may start another.
#[must_use]
pub const fn retry_allowed(attempts_started: u32) -> bool {
    attempts_started < MAX_EXECUTION_ATTEMPTS
}

/// Durable state of a task. Ready-queue membership is derived from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    /// Eligible for dispatch when its job and capabilities permit.
    Pending,
    /// One authoritative attempt holds a durable grant and lease.
    Leased,
    /// A recoverable failure is waiting for its `retry_not_before` time.
    RetryWaiting,
    /// One immutable receipt is durably accepted; no more attempts are allowed.
    Accepted,
    /// Unfinished work was fenced because its job stopped.
    Cancelled,
    /// A permanent failure or exhausted retry budget ended the task.
    Failed,
}

impl TaskState {
    /// Every state.
    pub const ALL: [Self; 6] = [
        Self::Pending,
        Self::Leased,
        Self::RetryWaiting,
        Self::Accepted,
        Self::Cancelled,
        Self::Failed,
    ];

    /// Returns the persisted name of the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Leased => "Leased",
            Self::RetryWaiting => "RetryWaiting",
            Self::Accepted => "Accepted",
            Self::Cancelled => "Cancelled",
            Self::Failed => "Failed",
        }
    }

    /// Parses a persisted name produced by [`Self::as_str`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == name)
    }

    /// Returns whether the state is terminal. `Accepted` stays permanent even
    /// if the job is later cancelled or needs replanning.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Accepted | Self::Cancelled | Self::Failed)
    }

    /// Checks whether the task may move from `self` to `to`.
    ///
    /// # Errors
    ///
    /// Returns [`TaskTransitionError`] when the transition graph has no such edge.
    pub const fn validate_transition(self, to: Self) -> Result<(), TaskTransitionError> {
        let allowed = match self {
            Self::Pending => matches!(to, Self::Leased | Self::Cancelled | Self::Failed),
            Self::Leased => matches!(
                to,
                Self::Accepted | Self::RetryWaiting | Self::Cancelled | Self::Failed
            ),
            Self::RetryWaiting => matches!(to, Self::Pending | Self::Cancelled | Self::Failed),
            Self::Accepted | Self::Cancelled | Self::Failed => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(TaskTransitionError { from: self, to })
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A task transition that the lifecycle rules forbid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("task cannot move from {from} to {to}")]
pub struct TaskTransitionError {
    /// Current state.
    pub from: TaskState,
    /// Requested state.
    pub to: TaskState,
}

/// State of one execution attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttemptState {
    /// A durable grant exists; no progress has been observed yet.
    Granted,
    /// The worker has reported progress.
    Running,
    /// The attempt's receipt is durably accepted.
    Accepted,
    /// The attempt failed.
    Failed,
    /// The lease expired before a result was accepted.
    Expired,
    /// The attempt was fenced by cancellation.
    Cancelled,
}

impl AttemptState {
    /// Every state.
    pub const ALL: [Self; 6] = [
        Self::Granted,
        Self::Running,
        Self::Accepted,
        Self::Failed,
        Self::Expired,
        Self::Cancelled,
    ];

    /// Returns the persisted name of the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "Granted",
            Self::Running => "Running",
            Self::Accepted => "Accepted",
            Self::Failed => "Failed",
            Self::Expired => "Expired",
            Self::Cancelled => "Cancelled",
        }
    }

    /// Parses a persisted name produced by [`Self::as_str`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == name)
    }

    /// Returns whether the state is terminal. Expired, failed, and cancelled
    /// attempts can never become authoritative again.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Granted | Self::Running)
    }

    /// Checks whether the attempt may move from `self` to `to`. A grant may
    /// move straight to `Accepted` when no progress was observed.
    ///
    /// # Errors
    ///
    /// Returns [`AttemptTransitionError`] when the transition graph has no such edge.
    pub const fn validate_transition(self, to: Self) -> Result<(), AttemptTransitionError> {
        let allowed = match self {
            Self::Granted => !matches!(to, Self::Granted),
            Self::Running => !matches!(to, Self::Granted | Self::Running),
            Self::Accepted | Self::Failed | Self::Expired | Self::Cancelled => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(AttemptTransitionError { from: self, to })
        }
    }
}

impl fmt::Display for AttemptState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An attempt transition that the lifecycle rules forbid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("attempt cannot move from {from} to {to}")]
pub struct AttemptTransitionError {
    /// Current state.
    pub from: AttemptState,
    /// Requested state.
    pub to: AttemptState,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn persisted_names_round_trip() {
        for state in TaskState::ALL {
            assert_eq!(TaskState::from_name(state.as_str()), Some(state));
        }
        for state in AttemptState::ALL {
            assert_eq!(AttemptState::from_name(state.as_str()), Some(state));
        }
        assert_eq!(TaskState::from_name("Granted"), None);
        assert_eq!(AttemptState::from_name("Pending"), None);
    }

    #[test]
    fn retry_budget_counts_the_original_attempt() {
        assert!(retry_allowed(0));
        assert!(retry_allowed(1));
        assert!(retry_allowed(2));
        assert!(!retry_allowed(3));
        assert!(!retry_allowed(u32::MAX));
    }

    #[test]
    fn task_transitions_match_the_plan() {
        use TaskState::*;
        let expected: HashSet<_> = [
            (Pending, Leased),
            (Pending, Cancelled),
            (Pending, Failed),
            (Leased, Accepted),
            (Leased, RetryWaiting),
            (Leased, Cancelled),
            (Leased, Failed),
            (RetryWaiting, Pending),
            (RetryWaiting, Cancelled),
            (RetryWaiting, Failed),
        ]
        .into_iter()
        .collect();
        for from in TaskState::ALL {
            for to in TaskState::ALL {
                let result = from.validate_transition(to);
                assert_eq!(
                    result.is_ok(),
                    expected.contains(&(from, to)),
                    "{from} -> {to}"
                );
                if let Err(err) = result {
                    assert_eq!(err, TaskTransitionError { from, to });
                }
            }
        }
    }

    #[test]
    fn task_terminal_states_have_no_exits() {
        for from in TaskState::ALL.into_iter().filter(|s| s.is_terminal()) {
            assert!(
                TaskState::ALL
                    .into_iter()
                    .all(|to| from.validate_transition(to).is_err())
            );
        }
        assert!(!TaskState::RetryWaiting.is_terminal());
    }

    #[test]
    fn attempt_transitions_match_the_plan() {
        use AttemptState::*;
        let expected: HashSet<_> = [
            (Granted, Running),
            (Granted, Accepted),
            (Granted, Failed),
            (Granted, Expired),
            (Granted, Cancelled),
            (Running, Accepted),
            (Running, Failed),
            (Running, Expired),
            (Running, Cancelled),
        ]
        .into_iter()
        .collect();
        for from in AttemptState::ALL {
            for to in AttemptState::ALL {
                let result = from.validate_transition(to);
                assert_eq!(
                    result.is_ok(),
                    expected.contains(&(from, to)),
                    "{from} -> {to}"
                );
                if let Err(err) = result {
                    assert_eq!(err, AttemptTransitionError { from, to });
                }
            }
        }
    }

    #[test]
    fn attempt_terminal_classification() {
        let terminal: Vec<_> = AttemptState::ALL
            .into_iter()
            .filter(|s| s.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            [
                AttemptState::Accepted,
                AttemptState::Failed,
                AttemptState::Expired,
                AttemptState::Cancelled
            ]
        );
    }

    #[test]
    fn names_are_stable_for_display_and_serde() {
        for state in TaskState::ALL {
            assert_eq!(state.to_string(), state.as_str());
            let back: TaskState =
                serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
            assert_eq!(back, state);
        }
        for state in AttemptState::ALL {
            assert_eq!(state.to_string(), state.as_str());
            let back: AttemptState =
                serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn errors_describe_the_transition() {
        let task = TaskState::Accepted
            .validate_transition(TaskState::Leased)
            .unwrap_err();
        assert_eq!(task.to_string(), "task cannot move from Accepted to Leased");
        let attempt = AttemptState::Expired
            .validate_transition(AttemptState::Running)
            .unwrap_err();
        assert_eq!(
            attempt.to_string(),
            "attempt cannot move from Expired to Running"
        );
    }
}
