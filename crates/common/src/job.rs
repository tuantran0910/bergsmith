//! Job lifecycle states and transition rules.
//!
//! Implements the state and transition tables in `.private/BERGSMITH_PROJECT_PLAN.md`
//! §2, "Job lifecycle" and "Cancellation contract". Persistence applies these
//! rules inside the revision-checked transaction that records each transition.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Durable state of a maintenance job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobState {
    /// Capturing and validating the planning state and building an immutable plan.
    Planning,
    /// The plan is durable; awaiting admission, capacity, or first assignment.
    Queued,
    /// Execution has started; tasks may be active, accepted, pending, or retrying.
    Running,
    /// Every task has an accepted receipt; publication checks are pending.
    ReadyToCommit,
    /// The immutable intent is durable; the commit is being validated, sent, or recovered.
    Committing,
    /// A commit may have happened and catalog evidence has not resolved the outcome.
    CommitOutcomeUnknown,
    /// Cancellation won before send authorization; dispatch and acceptance are fenced.
    Cancelling,
    /// The exact proposal is proven published.
    Succeeded,
    /// Planning found no eligible or beneficial work.
    NoOp,
    /// Attempts are fenced and no publication is outstanding.
    Cancelled,
    /// A planning or commit conflict requires a fresh plan and job.
    ReplanRequired,
    /// A permanent failure or exhausted retry budget ended the job.
    Failed,
}

impl JobState {
    /// Every state, in lifecycle order.
    pub const ALL: [Self; 12] = [
        Self::Planning,
        Self::Queued,
        Self::Running,
        Self::ReadyToCommit,
        Self::Committing,
        Self::CommitOutcomeUnknown,
        Self::Cancelling,
        Self::Succeeded,
        Self::NoOp,
        Self::Cancelled,
        Self::ReplanRequired,
        Self::Failed,
    ];

    /// Returns the persisted name of the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "Planning",
            Self::Queued => "Queued",
            Self::Running => "Running",
            Self::ReadyToCommit => "ReadyToCommit",
            Self::Committing => "Committing",
            Self::CommitOutcomeUnknown => "CommitOutcomeUnknown",
            Self::Cancelling => "Cancelling",
            Self::Succeeded => "Succeeded",
            Self::NoOp => "NoOp",
            Self::Cancelled => "Cancelled",
            Self::ReplanRequired => "ReplanRequired",
            Self::Failed => "Failed",
        }
    }

    /// Parses a persisted name produced by [`Self::as_str`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == name)
    }

    /// Returns whether the state is terminal. Terminal jobs never transition.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::NoOp | Self::Cancelled | Self::ReplanRequired | Self::Failed
        )
    }

    /// Returns whether the state ends the job without publishing.
    const fn is_terminal_non_success(self) -> bool {
        matches!(self, Self::Cancelled | Self::ReplanRequired | Self::Failed)
    }

    /// Returns whether the transition graph has an edge from `self` to `to`.
    const fn has_edge(self, to: Self) -> bool {
        match self {
            // Metadata-only jobs have no tasks to run, so their plan is ready
            // to commit as soon as it is durable.
            Self::Planning => matches!(
                to,
                Self::Queued
                    | Self::ReadyToCommit
                    | Self::NoOp
                    | Self::ReplanRequired
                    | Self::Failed
                    | Self::Cancelling
            ),
            Self::Queued => matches!(
                to,
                Self::Running | Self::ReplanRequired | Self::Failed | Self::Cancelling
            ),
            Self::Running => matches!(
                to,
                Self::ReadyToCommit | Self::ReplanRequired | Self::Failed | Self::Cancelling
            ),
            Self::ReadyToCommit => matches!(
                to,
                Self::Committing | Self::ReplanRequired | Self::Failed | Self::Cancelling
            ),
            Self::Committing => matches!(
                to,
                Self::Cancelling
                    | Self::Succeeded
                    | Self::CommitOutcomeUnknown
                    | Self::Cancelled
                    | Self::ReplanRequired
                    | Self::Failed
            ),
            Self::CommitOutcomeUnknown => matches!(
                to,
                Self::Succeeded
                    | Self::Committing
                    | Self::Cancelled
                    | Self::ReplanRequired
                    | Self::Failed
            ),
            Self::Cancelling => matches!(to, Self::Cancelled),
            Self::Succeeded
            | Self::NoOp
            | Self::Cancelled
            | Self::ReplanRequired
            | Self::Failed => false,
        }
    }

    /// Checks whether the job may move from `self` to `to`.
    ///
    /// Beyond the transition graph, this enforces the publication rules:
    /// cancellation cannot begin once a commit send is authorized, and after
    /// authorization a job reaches `Cancelled`, `ReplanRequired`, or `Failed`
    /// only with proof that nothing was published. A job in
    /// `CommitOutcomeUnknown` is always treated as send-authorized.
    ///
    /// # Errors
    ///
    /// Returns a [`JobTransitionError`] describing the rule the transition breaks.
    pub fn validate_transition(
        self,
        to: Self,
        context: PublicationContext,
    ) -> Result<(), JobTransitionError> {
        if self.is_terminal() {
            return Err(JobTransitionError::FromTerminal { from: self, to });
        }
        let send_authorized = context.send_authorized || self == Self::CommitOutcomeUnknown;
        if to == Self::Cancelling && send_authorized {
            return Err(JobTransitionError::CancellationAfterSendAuthorization { from: self });
        }
        if !self.has_edge(to) {
            return Err(JobTransitionError::NotAllowed { from: self, to });
        }
        if to.is_terminal_non_success() && send_authorized && !context.non_publication_proven {
            return Err(JobTransitionError::PublicationNotRuledOut { from: self, to });
        }
        Ok(())
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a maintenance job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobOperation {
    /// Rewrite data and metadata files to reclaim space.
    Compact,
    /// Remove snapshots that no live plan or publication pins.
    ExpireSnapshots,
}

impl JobOperation {
    /// Every operation.
    pub const ALL: [Self; 2] = [Self::Compact, Self::ExpireSnapshots];

    /// Returns the persisted name of the operation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::ExpireSnapshots => "ExpireSnapshots",
        }
    }

    /// Parses a persisted name produced by [`Self::as_str`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.as_str() == name)
    }
}

/// Publication facts that some job transitions depend on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicationContext {
    /// `publication_started_at` is set, so a commit request may have been sent.
    pub send_authorized: bool,
    /// Conclusive evidence shows that no authorized request committed or can commit.
    pub non_publication_proven: bool,
}

/// A job transition that the lifecycle rules forbid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JobTransitionError {
    /// The job is terminal; later work needs a new job.
    #[error("job state {from} is terminal and cannot move to {to}")]
    FromTerminal {
        /// Current state.
        from: JobState,
        /// Requested state.
        to: JobState,
    },
    /// The transition graph has no such edge.
    #[error("job cannot move from {from} to {to}")]
    NotAllowed {
        /// Current state.
        from: JobState,
        /// Requested state.
        to: JobState,
    },
    /// Cancellation was requested after the commit send was authorized.
    #[error("cancellation cannot begin after send authorization (job is {from})")]
    CancellationAfterSendAuthorization {
        /// Current state.
        from: JobState,
    },
    /// A non-success ending was requested without proof of non-publication.
    #[error("job cannot move from {from} to {to} until non-publication is proven")]
    PublicationNotRuledOut {
        /// Current state.
        from: JobState,
        /// Requested state.
        to: JobState,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::JobState::*;
    use super::*;

    /// The plan's transition table, with no publication in flight.
    fn expected_edges() -> HashSet<(JobState, JobState)> {
        [
            (Planning, Queued),
            (Planning, ReadyToCommit),
            (Planning, NoOp),
            (Planning, ReplanRequired),
            (Planning, Failed),
            (Planning, Cancelling),
            (Queued, Running),
            (Queued, ReplanRequired),
            (Queued, Failed),
            (Queued, Cancelling),
            (Running, ReadyToCommit),
            (Running, ReplanRequired),
            (Running, Failed),
            (Running, Cancelling),
            (ReadyToCommit, Committing),
            (ReadyToCommit, ReplanRequired),
            (ReadyToCommit, Failed),
            (ReadyToCommit, Cancelling),
            (Committing, Cancelling),
            (Committing, Succeeded),
            (Committing, CommitOutcomeUnknown),
            (Committing, Cancelled),
            (Committing, ReplanRequired),
            (Committing, Failed),
            (CommitOutcomeUnknown, Succeeded),
            (CommitOutcomeUnknown, Committing),
            (Cancelling, Cancelled),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn persisted_names_round_trip() {
        for state in JobState::ALL {
            assert_eq!(JobState::from_name(state.as_str()), Some(state));
        }
        assert_eq!(JobState::from_name("planning"), None);
    }

    #[test]
    fn operation_names_are_stable() {
        for operation in JobOperation::ALL {
            assert_eq!(JobOperation::from_name(operation.as_str()), Some(operation));
            let json = serde_json::to_string(&operation).unwrap();
            assert_eq!(json, format!("\"{}\"", operation.as_str()));
            let back: JobOperation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, operation);
        }
        assert_eq!(JobOperation::from_name("compact"), None);
    }

    #[test]
    fn transition_table_matches_the_plan_before_send_authorization() {
        let expected = expected_edges();
        for from in JobState::ALL {
            for to in JobState::ALL {
                let allowed = from
                    .validate_transition(to, PublicationContext::default())
                    .is_ok();
                assert_eq!(allowed, expected.contains(&(from, to)), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn terminal_states_never_transition() {
        for from in JobState::ALL.into_iter().filter(|s| s.is_terminal()) {
            for to in JobState::ALL {
                assert_eq!(
                    from.validate_transition(to, PublicationContext::default()),
                    Err(JobTransitionError::FromTerminal { from, to })
                );
            }
        }
    }

    #[test]
    fn cancellation_is_rejected_after_send_authorization() {
        let context = PublicationContext {
            send_authorized: true,
            non_publication_proven: false,
        };
        assert_eq!(
            Committing.validate_transition(Cancelling, context),
            Err(JobTransitionError::CancellationAfterSendAuthorization { from: Committing })
        );
    }

    #[test]
    fn unknown_outcome_never_enters_cancelling() {
        assert_eq!(
            CommitOutcomeUnknown.validate_transition(Cancelling, PublicationContext::default()),
            Err(JobTransitionError::CancellationAfterSendAuthorization {
                from: CommitOutcomeUnknown
            })
        );
    }

    #[test]
    fn non_success_after_send_requires_proof() {
        let unproven = PublicationContext {
            send_authorized: true,
            non_publication_proven: false,
        };
        let proven = PublicationContext {
            send_authorized: true,
            non_publication_proven: true,
        };
        for to in [Cancelled, ReplanRequired, Failed] {
            assert_eq!(
                Committing.validate_transition(to, unproven),
                Err(JobTransitionError::PublicationNotRuledOut {
                    from: Committing,
                    to
                })
            );
            assert_eq!(Committing.validate_transition(to, proven), Ok(()));
        }
    }

    #[test]
    fn unknown_outcome_counts_as_send_authorized() {
        for to in [Cancelled, ReplanRequired, Failed] {
            assert_eq!(
                CommitOutcomeUnknown.validate_transition(to, PublicationContext::default()),
                Err(JobTransitionError::PublicationNotRuledOut {
                    from: CommitOutcomeUnknown,
                    to
                })
            );
            let proven = PublicationContext {
                send_authorized: true,
                non_publication_proven: true,
            };
            assert_eq!(CommitOutcomeUnknown.validate_transition(to, proven), Ok(()));
        }
    }

    #[test]
    fn success_never_needs_non_publication_proof() {
        let context = PublicationContext {
            send_authorized: true,
            non_publication_proven: false,
        };
        assert_eq!(Committing.validate_transition(Succeeded, context), Ok(()));
        assert_eq!(
            CommitOutcomeUnknown.validate_transition(Succeeded, context),
            Ok(())
        );
    }

    #[test]
    fn terminal_classification_matches_the_plan() {
        let terminal: Vec<_> = JobState::ALL
            .into_iter()
            .filter(|s| s.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            [Succeeded, NoOp, Cancelled, ReplanRequired, Failed]
        );
    }

    #[test]
    fn names_are_stable_for_display_and_serde() {
        for state in JobState::ALL {
            assert_eq!(state.to_string(), state.as_str());
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{}\"", state.as_str()));
            let back: JobState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn errors_describe_the_rule() {
        let err = Succeeded
            .validate_transition(Running, PublicationContext::default())
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "job state Succeeded is terminal and cannot move to Running"
        );
    }
}
