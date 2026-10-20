//! The append-only audit log.
//!
//! Callers pass typed actions and outcomes; the detail object must contain only
//! identifiers and stable reason codes. Credentials never reach this module:
//! the schema rejects the obvious credential keys as a second line of defense.

use bergsmith_common::audit::{ActorKind, AuditOutcome};
use bergsmith_common::ids::{OrganizationId, PrincipalId};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;

/// One append-only audit record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// Monotonic identifier.
    pub id: i64,
    /// Owning organization, if the event belongs to one.
    pub organization_id: Option<OrganizationId>,
    /// Acting principal, if authenticated.
    pub actor_principal_id: Option<PrincipalId>,
    /// Kind of actor.
    pub actor_kind: ActorKind,
    /// Stable action name, such as `job.publication_denied`.
    pub action: String,
    /// Kind of resource, such as `job` or `catalog_connection`.
    pub resource_type: String,
    /// Resource identifier, if any.
    pub resource_id: Option<String>,
    /// Outcome.
    pub outcome: AuditOutcome,
    /// Structured, secret-free context.
    pub detail: serde_json::Value,
    /// When the event was recorded (database time).
    pub occurred_at: DateTime<Utc>,
}

/// Fields for a new audit record.
#[derive(Clone, Debug)]
pub struct NewAuditEvent<'a> {
    /// Owning organization, if the event belongs to one.
    pub organization_id: Option<OrganizationId>,
    /// Acting principal, if authenticated.
    pub actor_principal_id: Option<PrincipalId>,
    /// Kind of actor.
    pub actor_kind: ActorKind,
    /// Stable action name.
    pub action: &'a str,
    /// Kind of resource.
    pub resource_type: &'a str,
    /// Resource identifier, if any.
    pub resource_id: Option<&'a str>,
    /// Outcome.
    pub outcome: AuditOutcome,
    /// Structured, secret-free context.
    pub detail: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct AuditEventRow {
    id: i64,
    organization_id: Option<Uuid>,
    actor_principal_id: Option<Uuid>,
    actor_kind: String,
    action: String,
    resource_type: String,
    resource_id: Option<String>,
    outcome: String,
    detail: serde_json::Value,
    occurred_at: DateTime<Utc>,
}

impl TryFrom<AuditEventRow> for AuditEvent {
    type Error = PersistenceError;

    fn try_from(row: AuditEventRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            organization_id: row.organization_id.map(OrganizationId::from_uuid),
            actor_principal_id: row.actor_principal_id.map(PrincipalId::from_uuid),
            actor_kind: ActorKind::from_name(&row.actor_kind).ok_or_else(|| {
                PersistenceError::CorruptValue {
                    entity: "audit actor kind",
                    value: row.actor_kind.clone(),
                }
            })?,
            action: row.action,
            resource_type: row.resource_type,
            resource_id: row.resource_id,
            outcome: AuditOutcome::from_name(&row.outcome).ok_or_else(|| {
                PersistenceError::CorruptValue {
                    entity: "audit outcome",
                    value: row.outcome.clone(),
                }
            })?,
            detail: row.detail,
            occurred_at: row.occurred_at,
        })
    }
}

/// A transaction-scoped advisory lock that serializes audit appends, so a
/// lower identifier cannot commit after a reader has advanced past it.
const AUDIT_APPEND_LOCK: i64 = 0x0042_5341_5544_4954;

/// Columns selected for [`AuditEvent`], in [`AuditEventRow`] order. A macro so
/// queries stay `&'static str` literals.
macro_rules! audit_columns {
    () => {
        "id, organization_id, actor_principal_id, actor_kind, action, \
         resource_type, resource_id, outcome, detail, occurred_at"
    };
}

/// Appends an audit record and returns its identifier.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when a named organization or actor
/// does not exist, [`PersistenceError::InvalidValue`] for a disallowed detail
/// key, and database errors.
pub async fn record(
    conn: &mut PgConnection,
    event: &NewAuditEvent<'_>,
) -> Result<i64, PersistenceError> {
    let mut tx = conn.begin().await?;
    // Serialize appends: identity values follow allocation order, but a
    // cursor over committed rows must never advance past an event that is
    // still pending. Holding this lock until commit makes both orders agree.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AUDIT_APPEND_LOCK)
        .execute(&mut *tx)
        .await?;
    let id = sqlx::query_scalar(
        "INSERT INTO audit_events
             (organization_id, actor_principal_id, actor_kind, action, resource_type,
              resource_id, outcome, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id",
    )
    .bind(event.organization_id.as_ref().map(OrganizationId::as_uuid))
    .bind(event.actor_principal_id.as_ref().map(PrincipalId::as_uuid))
    .bind(event.actor_kind.as_str())
    .bind(event.action)
    .bind(event.resource_type)
    .bind(event.resource_id)
    .bind(event.outcome.as_str())
    .bind(&event.detail)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("audit event", error))?;
    tx.commit().await?;
    Ok(id)
}

/// Reads an organization's audit log in identifier order.
///
/// `after` is the last identifier the caller has seen; `limit` is capped at
/// 500 so one page stays bounded.
///
/// # Errors
///
/// Returns a database error if the query fails.
pub async fn list(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<AuditEvent>, PersistenceError> {
    let limit = limit.clamp(1, 500);
    let rows = sqlx::query_as::<_, AuditEventRow>(concat!(
        "SELECT ",
        audit_columns!(),
        " FROM audit_events
         WHERE organization_id = $1 AND ($2::bigint IS NULL OR id > $2)
         ORDER BY id LIMIT $3"
    ))
    .bind(organization_id.as_uuid())
    .bind(after)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}
