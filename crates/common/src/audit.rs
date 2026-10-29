//! Audit vocabulary shared by the coordinator and the ledger.
//!
//! Audit records are append-only. They name the actor, the action, the
//! resource, and the outcome, and never contain credentials, tokens, or object
//! contents. BS-017 records identity, grant, and maintenance decisions;
//! BS-018 exposes them through the API. See ADR 0032.

/// Who acted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorKind {
    /// A person authenticated with OIDC.
    User,
    /// A service principal, such as a schedule's maintenance identity.
    Service,
    /// A request that could not be authenticated.
    Anonymous,
    /// The coordinator itself, such as a recovery decision.
    System,
}

impl ActorKind {
    /// The stable name persisted in PostgreSQL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Service => "Service",
            Self::Anonymous => "Anonymous",
            Self::System => "System",
        }
    }

    /// Parses the persisted name.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "User" => Some(Self::User),
            "Service" => Some(Self::Service),
            "Anonymous" => Some(Self::Anonymous),
            "System" => Some(Self::System),
            _ => None,
        }
    }
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAction {
    /// An OIDC login completed and a session was issued.
    AuthLoginSucceeded,
    /// An OIDC login failed validation or identity binding.
    AuthLoginFailed,
    /// A session was revoked by an explicit logout.
    AuthLoggedOut,
    /// A request presented a missing, expired, or revoked session.
    AuthSessionDenied,
    /// A role grant was created.
    GrantCreated,
    /// A role grant was revoked.
    GrantRevoked,
    /// A session was revoked administratively.
    SessionRevoked,
    /// A principal was provisioned for a verified issuer and subject.
    IdentityProvisioned,
    /// A principal was deactivated; its sessions ended and its grants stay.
    PrincipalDeactivated,
    /// A previously deactivated principal was reactivated.
    PrincipalReactivated,
    /// A worker pool token was issued; the token itself is never recorded.
    WorkerTokenIssued,
    /// A worker pool token was revoked.
    WorkerTokenRevoked,
    /// A worker pool token was rotated; the predecessor was revoked.
    WorkerTokenRotated,
    /// Admission authorized a job.
    JobAdmitted,
    /// Admission refused a job because the requester's grant was revoked or
    /// never covered the table.
    JobAdmissionDenied,
    /// Publication was authorized for a job.
    JobPublished,
    /// Publication was refused because the requester no longer holds the
    /// required grant.
    JobPublicationDenied,
    /// Cancellation was requested.
    JobCancellationRequested,
    /// A fresh-plan retry was requested.
    JobRetryRequested,
    /// An operator attested that an unknown commit outcome did not publish.
    JobNonPublicationAttested,
    /// The public API durably accepted a new job submission.
    JobSubmitted,
    /// A maintenance policy was created.
    PolicyCreated,
    /// A maintenance policy was edited or enabled/disabled.
    PolicyUpdated,
    /// A schedule was created.
    ScheduleCreated,
    /// A schedule was edited, enabled, or disabled.
    ScheduleUpdated,
    /// A public API command was refused by scoped authorization.
    ApiPermissionDenied,
    /// A catalog connection was created.
    CatalogCreated,
    /// A catalog connection was edited, enabled, or disabled.
    CatalogUpdated,
    /// A catalog connection's capabilities were checked.
    CatalogChecked,
    /// A notification destination was created.
    NotificationDestinationCreated,
    /// A notification destination was edited, enabled, or disabled.
    NotificationDestinationUpdated,
    /// A test notification was sent to a destination.
    NotificationDestinationTested,
    /// A notification rule was created.
    NotificationRuleCreated,
    /// A notification rule was edited, enabled, or disabled.
    NotificationRuleUpdated,
    /// Catalog names were registered as tables.
    CatalogTablesRegistered,
    /// A table's catalog name was unregistered.
    CatalogTableUnregistered,
}

impl AuditAction {
    /// The stable dotted name persisted in PostgreSQL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthLoginSucceeded => "auth.login_succeeded",
            Self::AuthLoginFailed => "auth.login_failed",
            Self::AuthLoggedOut => "auth.logged_out",
            Self::AuthSessionDenied => "auth.session_denied",
            Self::GrantCreated => "grant.created",
            Self::GrantRevoked => "grant.revoked",
            Self::SessionRevoked => "session.revoked",
            Self::IdentityProvisioned => "identity.provisioned",
            Self::PrincipalDeactivated => "principal.deactivated",
            Self::PrincipalReactivated => "principal.reactivated",
            Self::WorkerTokenIssued => "worker_token.issued",
            Self::WorkerTokenRevoked => "worker_token.revoked",
            Self::WorkerTokenRotated => "worker_token.rotated",
            Self::JobAdmitted => "job.admitted",
            Self::JobAdmissionDenied => "job.admission_denied",
            Self::JobPublished => "job.published",
            Self::JobPublicationDenied => "job.publication_denied",
            Self::JobCancellationRequested => "job.cancellation_requested",
            Self::JobRetryRequested => "job.retry_requested",
            Self::JobNonPublicationAttested => "job.non_publication_attested",
            Self::JobSubmitted => "job.submitted",
            Self::PolicyCreated => "policy.created",
            Self::PolicyUpdated => "policy.updated",
            Self::ScheduleCreated => "schedule.created",
            Self::ScheduleUpdated => "schedule.updated",
            Self::ApiPermissionDenied => "api.permission_denied",
            Self::NotificationDestinationCreated => "notification.destination_created",
            Self::NotificationDestinationUpdated => "notification.destination_updated",
            Self::NotificationDestinationTested => "notification.destination_tested",
            Self::NotificationRuleCreated => "notification.rule_created",
            Self::NotificationRuleUpdated => "notification.rule_updated",
            Self::CatalogCreated => "catalog.created",
            Self::CatalogUpdated => "catalog.updated",
            Self::CatalogChecked => "catalog.checked",
            Self::CatalogTablesRegistered => "catalog.tables_registered",
            Self::CatalogTableUnregistered => "catalog.table_unregistered",
        }
    }

    /// Parses a persisted action name.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|action| action.as_str() == value)
    }

    /// Every action, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::AuthLoginSucceeded,
        Self::AuthLoginFailed,
        Self::AuthLoggedOut,
        Self::AuthSessionDenied,
        Self::GrantCreated,
        Self::GrantRevoked,
        Self::SessionRevoked,
        Self::IdentityProvisioned,
        Self::PrincipalDeactivated,
        Self::PrincipalReactivated,
        Self::WorkerTokenIssued,
        Self::WorkerTokenRevoked,
        Self::WorkerTokenRotated,
        Self::JobAdmitted,
        Self::JobAdmissionDenied,
        Self::JobPublished,
        Self::JobPublicationDenied,
        Self::JobCancellationRequested,
        Self::JobRetryRequested,
        Self::JobNonPublicationAttested,
        Self::JobSubmitted,
        Self::PolicyCreated,
        Self::PolicyUpdated,
        Self::ScheduleCreated,
        Self::ScheduleUpdated,
        Self::ApiPermissionDenied,
        Self::NotificationDestinationCreated,
        Self::NotificationDestinationUpdated,
        Self::NotificationDestinationTested,
        Self::NotificationRuleCreated,
        Self::NotificationRuleUpdated,
        Self::CatalogCreated,
        Self::CatalogUpdated,
        Self::CatalogChecked,
        Self::CatalogTablesRegistered,
        Self::CatalogTableUnregistered,
    ];
}

/// The result of the audited action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditOutcome {
    /// The action was permitted and completed.
    Allowed,
    /// Authorization refused the action.
    Denied,
    /// The action was permitted but failed for another reason.
    Failed,
}

impl AuditOutcome {
    /// The stable name persisted in PostgreSQL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "Allowed",
            Self::Denied => "Denied",
            Self::Failed => "Failed",
        }
    }

    /// Parses the persisted name.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "Allowed" => Some(Self::Allowed),
            "Denied" => Some(Self::Denied),
            "Failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_round_trip_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for action in AuditAction::ALL {
            assert!(seen.insert(action.as_str()));
            assert_eq!(AuditAction::from_name(action.as_str()), Some(*action));
        }
        assert_eq!(AuditAction::from_name("job.unknown"), None);
    }

    #[test]
    fn actor_and_outcome_names_are_stable() {
        assert_eq!(ActorKind::from_name("Service"), Some(ActorKind::Service));
        assert_eq!(
            AuditOutcome::from_name("Denied"),
            Some(AuditOutcome::Denied)
        );
        assert_eq!(ActorKind::from_name("service"), None);
        assert_eq!(AuditOutcome::from_name("allowed"), None);
    }
}
