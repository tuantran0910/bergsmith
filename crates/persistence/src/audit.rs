//! The append-only audit log.
//!
//! Callers pass typed actions and outcomes; the detail object must contain only
//! identifiers and stable reason codes. Credentials never reach this module:
//! the schema rejects the obvious credential keys as a second line of defense.

use std::collections::BTreeSet;

use bergsmith_common::audit::{ActorKind, AuditOutcome};
use bergsmith_common::ids::{OrganizationId, PrincipalId};
use bergsmith_common::search::SearchText;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection, Postgres, QueryBuilder};
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
    /// The acting principal's current display name, for display only; `None`
    /// when there is no actor principal.
    pub actor_display_name: Option<String>,
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
    actor_display_name: Option<String>,
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
            actor_display_name: row.actor_display_name,
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

/// A transaction-scoped advisory lock that serializes audit appends per
/// organization, so a lower identifier cannot commit after a reader has
/// advanced past it. Events with no organization are not returned by any
/// organization-scoped read, so they need no ordering lock.
const AUDIT_APPEND_LOCK_NAMESPACE: i64 = 0x0042_5341_5544_4954;

/// Columns selected for [`AuditEvent`] from `audit_events e`, in
/// [`AuditEventRow`] order. A macro so queries stay `&'static str` literals.
/// The display name is read at query time, so it follows renames, and only
/// from the event's own organization.
macro_rules! audit_columns {
    () => {
        "e.id, e.organization_id, e.actor_principal_id, \
         (SELECT p.display_name FROM principals p \
           WHERE p.id = e.actor_principal_id AND p.organization_id = e.organization_id) \
           AS actor_display_name, \
         e.actor_kind, e.action, e.resource_type, e.resource_id, e.outcome, e.detail, \
         e.occurred_at"
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
    // Serialize appends per organization: identity values follow allocation
    // order, but a cursor over committed rows must never advance past an event
    // that is still pending. Holding the lock until commit makes both orders
    // agree. Other organizations and the audit log's org-less events cannot
    // affect a tenant's listing, so they do not contend for this lock.
    if let Some(organization_id) = event.organization_id {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
            .bind(organization_id.as_uuid().to_string())
            .bind(AUDIT_APPEND_LOCK_NAMESPACE)
            .execute(&mut *tx)
            .await?;
    }
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

/// Which audit records a list or a count keeps (BS-033.T4).
///
/// Empty sets and `None` keep everything. `actor_name` matches the actor's
/// current display name, never the record's detail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Keep these outcomes.
    pub outcomes: BTreeSet<AuditOutcome>,
    /// Keep these stable action names.
    pub actions: BTreeSet<String>,
    /// Keep these resource kinds.
    pub resource_types: BTreeSet<String>,
    /// Keep records by this actor.
    pub actor: Option<PrincipalId>,
    /// Keep records whose actor's display name contains this text.
    pub actor_name: Option<SearchText>,
    /// List the most recent record first.
    pub newest_first: bool,
}

/// An audit facet dimension, whose own filter its counts ignore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuditFacet {
    Outcome,
    Action,
    ResourceType,
}

impl AuditFilter {
    fn push(&self, query: &mut QueryBuilder<Postgres>, except: Option<AuditFacet>) {
        if except != Some(AuditFacet::Outcome) && !self.outcomes.is_empty() {
            let outcomes: Vec<&str> = self
                .outcomes
                .iter()
                .map(|outcome| outcome.as_str())
                .collect();
            query
                .push(" AND e.outcome = ANY(")
                .push_bind(outcomes)
                .push(")");
        }
        if except != Some(AuditFacet::Action) && !self.actions.is_empty() {
            let actions: Vec<&str> = self.actions.iter().map(String::as_str).collect();
            query
                .push(" AND e.action = ANY(")
                .push_bind(actions)
                .push(")");
        }
        if except != Some(AuditFacet::ResourceType) && !self.resource_types.is_empty() {
            let types: Vec<&str> = self.resource_types.iter().map(String::as_str).collect();
            query
                .push(" AND e.resource_type = ANY(")
                .push_bind(types)
                .push(")");
        }
        if let Some(actor) = self.actor {
            query
                .push(" AND e.actor_principal_id = ")
                .push_bind(*actor.as_uuid());
        }
        if let Some(name) = &self.actor_name {
            query
                .push(
                    " AND EXISTS (SELECT 1 FROM principals p \
                     WHERE p.id = e.actor_principal_id AND p.organization_id = e.organization_id \
                       AND p.display_name ILIKE ",
                )
                .push_bind(name.contains_pattern())
                .push(" ESCAPE '\\')");
        }
    }
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
    list_filtered(conn, organization_id, &AuditFilter::default(), after, limit).await
}

/// Reads the records of an organization's audit log that `filter` keeps, in
/// identifier order, oldest first unless the filter asks for the newest.
///
/// `after` is the last identifier the caller has seen; `limit` is capped at
/// 500 so one page stays bounded.
///
/// # Errors
///
/// Returns a database error if the query fails.
pub async fn list_filtered(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    filter: &AuditFilter,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<AuditEvent>, PersistenceError> {
    let limit = limit.clamp(1, 500);
    let mut query = QueryBuilder::new(concat!(
        "SELECT ",
        audit_columns!(),
        " FROM audit_events e WHERE e.organization_id = "
    ));
    query.push_bind(*organization_id.as_uuid());
    filter.push(&mut query, None);
    // Each direction keeps the cursor a direct index range condition.
    if let Some(after) = after {
        query
            .push(if filter.newest_first {
                " AND e.id < "
            } else {
                " AND e.id > "
            })
            .push_bind(after);
    }
    query
        .push(if filter.newest_first {
            " ORDER BY e.id DESC LIMIT "
        } else {
            " ORDER BY e.id ASC LIMIT "
        })
        .push_bind(limit);
    let rows: Vec<AuditEventRow> = query.build_query_as().fetch_all(conn).await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Counts of an organization's audit records by outcome, action, and
/// resource kind. Each dimension applies every filter but its own, and lists
/// its values in name order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditFacets {
    /// Records per outcome.
    pub outcome: Vec<(AuditOutcome, i64)>,
    /// Records per action name.
    pub action: Vec<(String, i64)>,
    /// Records per resource kind.
    pub resource_type: Vec<(String, i64)>,
}

/// Counts the records `filter` keeps along each facet dimension.
///
/// # Errors
///
/// [`PersistenceError::CorruptValue`] for an unknown stored outcome, and
/// database errors.
pub async fn facets(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    filter: &AuditFilter,
) -> Result<AuditFacets, PersistenceError> {
    let outcome = count(conn, organization_id, filter, AuditFacet::Outcome).await?;
    let action = count(conn, organization_id, filter, AuditFacet::Action).await?;
    let resource_type = count(conn, organization_id, filter, AuditFacet::ResourceType).await?;
    let outcome = outcome
        .into_iter()
        .map(|(name, count)| {
            AuditOutcome::from_name(&name)
                .map(|outcome| (outcome, count))
                .ok_or(PersistenceError::CorruptValue {
                    entity: "audit outcome",
                    value: name,
                })
        })
        .collect::<Result<_, _>>()?;
    Ok(AuditFacets {
        outcome,
        action,
        resource_type,
    })
}

async fn count(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    filter: &AuditFilter,
    facet: AuditFacet,
) -> Result<Vec<(String, i64)>, PersistenceError> {
    let column = match facet {
        AuditFacet::Outcome => "e.outcome",
        AuditFacet::Action => "e.action",
        AuditFacet::ResourceType => "e.resource_type",
    };
    let mut query = QueryBuilder::new("SELECT ");
    query
        .push(column)
        .push(", count(*) FROM audit_events e WHERE e.organization_id = ")
        .push_bind(*organization_id.as_uuid());
    filter.push(&mut query, Some(facet));
    query
        .push(" GROUP BY ")
        .push(column)
        .push(" ORDER BY ")
        .push(column);
    Ok(query.build_query_as().fetch_all(conn).await?)
}
