//! Typed identifiers.
//!
//! Every durable entity has its own identifier type, so a [`TaskId`] can never
//! be passed where an [`AttemptId`] is expected. Identifiers are version 7 UUIDs, which
//! sort by creation time.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Error returned when text does not parse as an identifier.
#[derive(Debug, thiserror::Error)]
#[error("invalid {kind}: {source}")]
pub struct IdParseError {
    kind: &'static str,
    #[source]
    source: uuid::Error,
}

impl IdParseError {
    /// Name of the identifier type that failed to parse.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.kind
    }
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new time-ordered identifier.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID.
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self).map_err(|source| IdParseError {
                    kind: stringify!($name),
                    source,
                })
            }
        }
    };
}

define_id!(
    /// Identifies an organization, the scope for idempotency keys and grants.
    OrganizationId
);
define_id!(
    /// Identifies a principal: a person or a service account.
    PrincipalId
);
define_id!(
    /// Identifies a person's saved list view (BS-033.T6).
    SavedViewId
);
define_id!(
    /// Identifies a connection to an Iceberg REST catalog.
    CatalogConnectionId
);
define_id!(
    /// Identifies a canonical table. Catalog aliases that name the same table
    /// resolve to one `TableId`, so they share coordination.
    TableId
);
define_id!(
    /// Identifies a maintenance job.
    JobId
);
define_id!(
    /// Identifies a job's immutable execution plan.
    PlanId
);
define_id!(
    /// Identifies one task within a plan.
    TaskId
);
define_id!(
    /// Identifies one execution attempt of a task.
    AttemptId
);
define_id!(
    /// Identifies an authenticated worker session.
    WorkerSessionId
);
define_id!(
    /// Identifies a job's single publication operation.
    PublicationId
);
define_id!(
    /// Identifies one role grant.
    RoleGrantId
);
define_id!(
    /// Identifies one worker pool bearer-token verifier.
    WorkerPoolTokenId
);
define_id!(
    /// Identifies a named maintenance policy.
    PolicyId
);
define_id!(
    /// Identifies a schedule that runs one policy against one table.
    ScheduleId
);
define_id!(
    /// Identifies a notification destination.
    NotificationDestinationId
);
define_id!(
    /// Identifies a notification rule.
    NotificationRuleId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique() {
        let a = JobId::generate();
        let b = JobId::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn display_and_parse_round_trip() {
        let id = TaskId::generate();
        let parsed: TaskId = id.to_string().parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn from_uuid_preserves_value() {
        let uuid = Uuid::now_v7();
        assert_eq!(AttemptId::from_uuid(uuid).as_uuid(), &uuid);
    }

    #[test]
    fn parse_error_names_the_type() {
        let err = "not-a-uuid".parse::<PlanId>().unwrap_err();
        assert_eq!(err.kind(), "PlanId");
        assert!(err.to_string().starts_with("invalid PlanId"));
    }

    #[test]
    fn serializes_as_a_bare_uuid_string() {
        let id = PublicationId::generate();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        let back: PublicationId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn every_id_type_parses() {
        let text = Uuid::now_v7().to_string();
        assert!(text.parse::<OrganizationId>().is_ok());
        assert!(text.parse::<WorkerSessionId>().is_ok());
        assert!(text.parse::<PrincipalId>().is_ok());
        assert!(text.parse::<CatalogConnectionId>().is_ok());
        assert!(text.parse::<TableId>().is_ok());
        assert!(text.parse::<PolicyId>().is_ok());
        assert!(text.parse::<ScheduleId>().is_ok());
        assert!(text.parse::<WorkerPoolTokenId>().is_ok());
    }
}
