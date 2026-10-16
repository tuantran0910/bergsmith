//! Failure dispositions.
//!
//! Implements `.private/BERGSMITH_PROJECT_PLAN.md` §2, "Failure and retry dispositions".
//! Workers report typed failures; the coordinator assigns the authoritative
//! disposition.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::job::JobState;

/// How the coordinator handles a classified failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureDisposition {
    /// Repeat the same request ID or immutable receipt; never start a new attempt.
    RetrySameRequest,
    /// Fence the old attempt and retry the same task with a new attempt, within budget.
    RetryTask,
    /// The planning baseline no longer holds; end the job and require a fresh plan.
    ReplanRequired,
    /// The failure cannot succeed on retry; end the job as failed.
    PermanentFailure,
    /// A commit may have happened; keep the intent and resolve it from evidence.
    ResolvePublication,
}

impl FailureDisposition {
    /// Every disposition.
    pub const ALL: [Self; 5] = [
        Self::RetrySameRequest,
        Self::RetryTask,
        Self::ReplanRequired,
        Self::PermanentFailure,
        Self::ResolvePublication,
    ];

    /// Returns the persisted name of the disposition.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetrySameRequest => "RetrySameRequest",
            Self::RetryTask => "RetryTask",
            Self::ReplanRequired => "ReplanRequired",
            Self::PermanentFailure => "PermanentFailure",
            Self::ResolvePublication => "ResolvePublication",
        }
    }

    /// Returns whether handling the failure starts a new execution attempt.
    #[must_use]
    pub const fn consumes_execution_attempt(self) -> bool {
        matches!(self, Self::RetryTask)
    }

    /// Returns the job state this disposition drives the job toward, if any.
    ///
    /// The move still goes through [`JobState::validate_transition`]; for
    /// example, `ReplanRequired` after send authorization waits for proof of
    /// non-publication.
    #[must_use]
    pub const fn target_job_state(self) -> Option<JobState> {
        match self {
            Self::RetrySameRequest | Self::RetryTask => None,
            Self::ReplanRequired => Some(JobState::ReplanRequired),
            Self::PermanentFailure => Some(JobState::Failed),
            Self::ResolvePublication => Some(JobState::CommitOutcomeUnknown),
        }
    }
}

impl fmt::Display for FailureDisposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_task_retries_consume_an_attempt() {
        let consuming: Vec<_> = FailureDisposition::ALL
            .into_iter()
            .filter(|d| d.consumes_execution_attempt())
            .collect();
        assert_eq!(consuming, [FailureDisposition::RetryTask]);
    }

    #[test]
    fn target_job_states_match_the_plan() {
        use FailureDisposition::*;
        assert_eq!(RetrySameRequest.target_job_state(), None);
        assert_eq!(RetryTask.target_job_state(), None);
        assert_eq!(
            ReplanRequired.target_job_state(),
            Some(JobState::ReplanRequired)
        );
        assert_eq!(PermanentFailure.target_job_state(), Some(JobState::Failed));
        assert_eq!(
            ResolvePublication.target_job_state(),
            Some(JobState::CommitOutcomeUnknown)
        );
    }

    #[test]
    fn names_are_stable_for_display_and_serde() {
        for disposition in FailureDisposition::ALL {
            assert_eq!(disposition.to_string(), disposition.as_str());
            let json = serde_json::to_string(&disposition).unwrap();
            assert_eq!(json, format!("\"{}\"", disposition.as_str()));
            let back: FailureDisposition = serde_json::from_str(&json).unwrap();
            assert_eq!(back, disposition);
        }
    }
}
