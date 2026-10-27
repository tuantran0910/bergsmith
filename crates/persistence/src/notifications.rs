//! Notification destinations (BS-029.T1, ADR 0060).
//!
//! A destination is an organization-scoped webhook or Slack-compatible
//! endpoint. It holds a URL reference and an optional signing-secret reference,
//! plus an optimistic `revision`, so two editors cannot silently overwrite each
//! other. The URL policy (HTTPS plus the deployment allow-list) is validated on
//! the resolved URL before it reaches this module; both stored fields are names,
//! never values.

use bergsmith_common::ids::{
    CatalogConnectionId, JobId, NotificationDestinationId, NotificationRuleId, OrganizationId,
    PrincipalId, TableId,
};
use bergsmith_common::notifications::{DestinationKind, EventKind};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};

use crate::PersistenceError;

/// One registered notification destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationDestination {
    /// Identifier.
    pub id: NotificationDestinationId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Transport shape.
    pub kind: DestinationKind,
    /// Name of the destination URL under `notifications.secrets_dir`.
    pub url_ref: String,
    /// Name of the signing secret under `notifications.secrets_dir`. Present
    /// exactly for a webhook.
    pub signing_secret_ref: Option<String>,
    /// Whether rules may deliver through it.
    pub enabled: bool,
    /// Optimistic concurrency revision.
    pub revision: i64,
    /// Who created it.
    pub created_by: PrincipalId,
    /// When it was created (database time).
    pub created_at: DateTime<Utc>,
    /// When it was last changed (database time).
    pub updated_at: DateTime<Utc>,
}

/// Fields for a new destination.
#[derive(Clone, Debug)]
pub struct NewNotificationDestination {
    /// Identifier to assign.
    pub id: NotificationDestinationId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Transport shape.
    pub kind: DestinationKind,
    /// Destination URL reference: resolved and validated at send time.
    pub url_ref: String,
    /// Signing-secret reference; required for a webhook, absent for Slack.
    pub signing_secret_ref: Option<String>,
    /// Whether it starts enabled.
    pub enabled: bool,
    /// Who is creating it.
    pub created_by: PrincipalId,
}

/// A destination update, applied only when the stored revision matches.
#[derive(Clone, Debug, Default)]
pub struct NotificationDestinationUpdate {
    /// New name, if it changes.
    pub name: Option<String>,
    /// New URL reference, if it changes.
    pub url_ref: Option<String>,
    /// New signing-secret reference, if it changes.
    pub signing_secret_ref: Option<String>,
    /// Whether it is enabled, if that changes.
    pub enabled: Option<bool>,
    /// The revision the caller read.
    pub expected_revision: i64,
}

macro_rules! destination_columns {
    () => {
        "id, organization_id, name, kind, url_ref, signing_secret_ref, enabled, \
         revision, created_by, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct DestinationRow {
    id: uuid::Uuid,
    organization_id: uuid::Uuid,
    name: String,
    kind: String,
    url_ref: String,
    signing_secret_ref: Option<String>,
    enabled: bool,
    revision: i64,
    created_by: uuid::Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<DestinationRow> for NotificationDestination {
    type Error = PersistenceError;

    fn try_from(row: DestinationRow) -> Result<Self, Self::Error> {
        let kind = DestinationKind::from_name(&row.kind).ok_or_else(|| {
            PersistenceError::InvalidValue {
                entity: "notification destination",
                constraint: format!("unknown kind {}", row.kind),
            }
        })?;
        Ok(Self {
            id: NotificationDestinationId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            name: row.name,
            kind,
            url_ref: row.url_ref,
            signing_secret_ref: row.signing_secret_ref,
            enabled: row.enabled,
            revision: row.revision,
            created_by: PrincipalId::from_uuid(row.created_by),
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// Creates a destination.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the organization already has a
/// destination with that name, and database errors.
pub async fn create(
    conn: &mut PgConnection,
    new: &NewNotificationDestination,
) -> Result<NotificationDestination, PersistenceError> {
    let row = sqlx::query_as::<_, DestinationRow>(concat!(
        "INSERT INTO notification_destinations
             (id, organization_id, name, kind, url_ref, signing_secret_ref, enabled,
              created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING ",
        destination_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(&new.name)
    .bind(new.kind.as_str())
    .bind(&new.url_ref)
    .bind(new.signing_secret_ref.as_deref())
    .bind(new.enabled)
    .bind(new.created_by.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("notification destination", error))?;
    row.try_into()
}

/// Reads one destination.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such
/// destination, and database errors.
pub async fn destination(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationDestinationId,
) -> Result<NotificationDestination, PersistenceError> {
    let row = sqlx::query_as::<_, DestinationRow>(concat!(
        "SELECT ",
        destination_columns!(),
        " FROM notification_destinations
         WHERE organization_id = $1 AND id = $2"
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "notification destination",
        id: id.to_string(),
    })?;
    row.try_into()
}

/// Lists destinations after `after`, in identifier order.
///
/// # Errors
///
/// Database errors.
pub async fn list(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    after: Option<NotificationDestinationId>,
    limit: i64,
) -> Result<Vec<NotificationDestination>, PersistenceError> {
    let rows = sqlx::query_as::<_, DestinationRow>(concat!(
        "SELECT ",
        destination_columns!(),
        " FROM notification_destinations
         WHERE organization_id = $1 AND ($2::uuid IS NULL OR id > $2)
         ORDER BY id
         LIMIT $3"
    ))
    .bind(organization_id.as_uuid())
    .bind(after.map(|after| *after.as_uuid()))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// The destinations enabled for an organization, for rule matching.
///
/// # Errors
///
/// Database errors.
pub async fn enabled(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
) -> Result<Vec<NotificationDestination>, PersistenceError> {
    let rows = sqlx::query_as::<_, DestinationRow>(concat!(
        "SELECT ",
        destination_columns!(),
        " FROM notification_destinations
         WHERE organization_id = $1 AND enabled
         ORDER BY id"
    ))
    .bind(organization_id.as_uuid())
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Applies a destination update.
///
/// The kind never changes; a webhook keeps a signing-secret reference and a
/// Slack destination keeps none.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the destination is missing,
/// [`PersistenceError::ConfigRevisionConflict`] when the revision does not
/// match, [`PersistenceError::AlreadyExists`] when the new name is taken, and
/// database errors.
pub async fn update(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationDestinationId,
    update: &NotificationDestinationUpdate,
) -> Result<NotificationDestination, PersistenceError> {
    let row = sqlx::query_as::<_, DestinationRow>(concat!(
        "UPDATE notification_destinations
         SET name = COALESCE($3, name),
             url_ref = COALESCE($4, url_ref),
             signing_secret_ref = COALESCE($5, signing_secret_ref),
             enabled = COALESCE($6, enabled),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND id = $2 AND revision = $7
         RETURNING ",
        destination_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .bind(update.name.as_deref())
    .bind(update.url_ref.as_deref())
    .bind(update.signing_secret_ref.as_deref())
    .bind(update.enabled)
    .bind(update.expected_revision)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("notification destination", error))?;
    match row {
        Some(row) => row.try_into(),
        None => {
            Err(revision_or_missing(conn, organization_id, id, update.expected_revision).await?)
        }
    }
}

async fn revision_or_missing(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationDestinationId,
    expected: i64,
) -> Result<PersistenceError, PersistenceError> {
    let actual: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM notification_destinations
         WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    match actual {
        Some(actual) => Ok(PersistenceError::ConfigRevisionConflict {
            entity: "notification destination",
            id: id.to_string(),
            expected,
            actual,
        }),
        None => Ok(PersistenceError::NotFound {
            entity: "notification destination",
            id: id.to_string(),
        }),
    }
}

/// One notification rule: an event kind narrowed to a destination and an
/// optional connection, pool, or table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationRule {
    /// Identifier.
    pub id: NotificationRuleId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Destination the rule delivers through.
    pub destination_id: NotificationDestinationId,
    /// Event kind the rule watches.
    pub event_kind: EventKind,
    /// Optional connection filter.
    pub catalog_connection_id: Option<CatalogConnectionId>,
    /// Optional worker-pool name filter.
    pub worker_pool: Option<String>,
    /// Optional table filter.
    pub table_identity_id: Option<TableId>,
    /// Notify only after this many occurrences; `None` means every event.
    pub repeat_threshold: Option<i32>,
    /// Whether the rule is active.
    pub enabled: bool,
    /// Optimistic concurrency revision.
    pub revision: i64,
    /// Who created it.
    pub created_by: PrincipalId,
    /// When it was created (database time).
    pub created_at: DateTime<Utc>,
    /// When it was last changed (database time).
    pub updated_at: DateTime<Utc>,
}

/// The one optional narrowing filter a rule may carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleFilter {
    /// Only events about this catalog connection.
    Connection(CatalogConnectionId),
    /// Only events about this worker pool.
    Pool(String),
    /// Only events about this table.
    Table(TableId),
}

impl RuleFilter {
    fn columns(&self) -> (Option<uuid::Uuid>, Option<&str>, Option<uuid::Uuid>) {
        match self {
            Self::Connection(id) => (Some(*id.as_uuid()), None, None),
            Self::Pool(pool) => (None, Some(pool.as_str()), None),
            Self::Table(id) => (None, None, Some(*id.as_uuid())),
        }
    }
}

/// Fields for a new rule.
#[derive(Clone, Debug)]
pub struct NewNotificationRule {
    /// Identifier to assign.
    pub id: NotificationRuleId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Destination the rule delivers through.
    pub destination_id: NotificationDestinationId,
    /// Event kind the rule watches.
    pub event_kind: EventKind,
    /// Optional narrowing filter.
    pub filter: Option<RuleFilter>,
    /// Notify only after this many occurrences.
    pub repeat_threshold: Option<i32>,
    /// Whether it starts enabled.
    pub enabled: bool,
    /// Who is creating it.
    pub created_by: PrincipalId,
}

/// A rule update, applied only when the stored revision matches.
///
/// The event kind never changes. `filter` and `repeat_threshold` are double
/// options: absent keeps the stored value, `Some(None)` clears it, and
/// `Some(Some(_))` replaces it.
#[derive(Clone, Debug, Default)]
pub struct NotificationRuleUpdate {
    /// New name, if it changes.
    pub name: Option<String>,
    /// New destination, if it changes.
    pub destination_id: Option<NotificationDestinationId>,
    /// New filter state, if it changes.
    pub filter: Option<Option<RuleFilter>>,
    /// New repeat threshold, if it changes.
    pub repeat_threshold: Option<Option<i32>>,
    /// Whether it is enabled, if that changes.
    pub enabled: Option<bool>,
    /// The revision the caller read.
    pub expected_revision: i64,
}

macro_rules! rule_columns {
    () => {
        "id, organization_id, name, destination_id, event_kind,
         catalog_connection_id, worker_pool, table_identity_id, repeat_threshold,
         enabled, revision, created_by, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: uuid::Uuid,
    organization_id: uuid::Uuid,
    name: String,
    destination_id: uuid::Uuid,
    event_kind: String,
    catalog_connection_id: Option<uuid::Uuid>,
    worker_pool: Option<String>,
    table_identity_id: Option<uuid::Uuid>,
    repeat_threshold: Option<i32>,
    enabled: bool,
    revision: i64,
    created_by: uuid::Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<RuleRow> for NotificationRule {
    type Error = PersistenceError;

    fn try_from(row: RuleRow) -> Result<Self, Self::Error> {
        let event_kind = EventKind::from_name(&row.event_kind).ok_or_else(|| {
            PersistenceError::InvalidValue {
                entity: "notification rule",
                constraint: format!("unknown event kind {}", row.event_kind),
            }
        })?;
        Ok(Self {
            id: NotificationRuleId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            name: row.name,
            destination_id: NotificationDestinationId::from_uuid(row.destination_id),
            event_kind,
            catalog_connection_id: row
                .catalog_connection_id
                .map(CatalogConnectionId::from_uuid),
            worker_pool: row.worker_pool,
            table_identity_id: row.table_identity_id.map(TableId::from_uuid),
            repeat_threshold: row.repeat_threshold,
            enabled: row.enabled,
            revision: row.revision,
            created_by: PrincipalId::from_uuid(row.created_by),
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

async fn require_destination(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationDestinationId,
) -> Result<(), PersistenceError> {
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM notification_destinations
         WHERE organization_id = $1 AND id = $2)",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    if found {
        Ok(())
    } else {
        Err(PersistenceError::NotFound {
            entity: "notification destination",
            id: id.to_string(),
        })
    }
}

async fn require_filter_target(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    filter: &RuleFilter,
) -> Result<(), PersistenceError> {
    match filter {
        RuleFilter::Pool(_) => Ok(()),
        RuleFilter::Connection(id) => {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM catalog_connections
                 WHERE organization_id = $1 AND id = $2)",
            )
            .bind(organization_id.as_uuid())
            .bind(id.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
            if found {
                Ok(())
            } else {
                Err(PersistenceError::NotFound {
                    entity: "catalog connection",
                    id: id.to_string(),
                })
            }
        }
        RuleFilter::Table(id) => {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM table_identities
                 WHERE organization_id = $1 AND id = $2)",
            )
            .bind(organization_id.as_uuid())
            .bind(id.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
            if found {
                Ok(())
            } else {
                Err(PersistenceError::NotFound {
                    entity: "table identity",
                    id: id.to_string(),
                })
            }
        }
    }
}

/// Creates a rule.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the destination or the filter target is
/// not in the organization, [`PersistenceError::AlreadyExists`] when the name
/// is taken, and database errors.
pub async fn create_rule(
    conn: &mut PgConnection,
    new: &NewNotificationRule,
) -> Result<NotificationRule, PersistenceError> {
    require_destination(conn, new.organization_id, new.destination_id).await?;
    if let Some(filter) = &new.filter {
        require_filter_target(conn, new.organization_id, filter).await?;
    }
    let (connection_id, pool, table_id) = match &new.filter {
        Some(filter) => filter.columns(),
        None => (None, None, None),
    };
    let row = sqlx::query_as::<_, RuleRow>(concat!(
        "INSERT INTO notification_rules
             (id, organization_id, name, destination_id, event_kind,
              catalog_connection_id, worker_pool, table_identity_id,
              repeat_threshold, enabled, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         RETURNING ",
        rule_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(&new.name)
    .bind(new.destination_id.as_uuid())
    .bind(new.event_kind.as_str())
    .bind(connection_id)
    .bind(pool)
    .bind(table_id)
    .bind(new.repeat_threshold)
    .bind(new.enabled)
    .bind(new.created_by.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("notification rule", error))?;
    row.try_into()
}

/// Reads one rule.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such rule, and
/// database errors.
pub async fn rule(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationRuleId,
) -> Result<NotificationRule, PersistenceError> {
    let row = sqlx::query_as::<_, RuleRow>(concat!(
        "SELECT ",
        rule_columns!(),
        " FROM notification_rules
         WHERE organization_id = $1 AND id = $2"
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "notification rule",
        id: id.to_string(),
    })?;
    row.try_into()
}

/// Lists rules after `after`, in identifier order.
///
/// # Errors
///
/// Database errors.
pub async fn rules(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    after: Option<NotificationRuleId>,
    limit: i64,
) -> Result<Vec<NotificationRule>, PersistenceError> {
    let rows = sqlx::query_as::<_, RuleRow>(concat!(
        "SELECT ",
        rule_columns!(),
        " FROM notification_rules
         WHERE organization_id = $1 AND ($2::uuid IS NULL OR id > $2)
         ORDER BY id
         LIMIT $3"
    ))
    .bind(organization_id.as_uuid())
    .bind(after.map(|after| *after.as_uuid()))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Applies a rule update.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the rule or a referenced row is
/// missing, [`PersistenceError::ConfigRevisionConflict`] when the revision
/// does not match, [`PersistenceError::AlreadyExists`] when the new name is
/// taken, and database errors.
pub async fn update_rule(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationRuleId,
    update: &NotificationRuleUpdate,
) -> Result<NotificationRule, PersistenceError> {
    if let Some(destination) = update.destination_id {
        require_destination(conn, organization_id, destination).await?;
    }
    if let Some(Some(filter)) = &update.filter {
        require_filter_target(conn, organization_id, filter).await?;
    }
    let (filter_changed, connection_id, pool, table_id) = match &update.filter {
        Some(Some(filter)) => {
            let (connection, pool, table) = filter.columns();
            (true, connection, pool, table)
        }
        Some(None) => (true, None, None, None),
        None => (false, None, None, None),
    };
    let (threshold_changed, threshold) = match update.repeat_threshold {
        Some(Some(value)) => (true, Some(value)),
        Some(None) => (true, None),
        None => (false, None),
    };
    let row = sqlx::query_as::<_, RuleRow>(concat!(
        "UPDATE notification_rules
         SET name = COALESCE($3, name),
             destination_id = COALESCE($4, destination_id),
             catalog_connection_id =
                 CASE WHEN $5 THEN $6::uuid ELSE catalog_connection_id END,
             worker_pool = CASE WHEN $5 THEN $7::text ELSE worker_pool END,
             table_identity_id =
                 CASE WHEN $5 THEN $8::uuid ELSE table_identity_id END,
             repeat_threshold =
                 CASE WHEN $9 THEN $10::integer ELSE repeat_threshold END,
             enabled = COALESCE($11, enabled),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND id = $2 AND revision = $12
         RETURNING ",
        rule_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .bind(update.name.as_deref())
    .bind(
        update
            .destination_id
            .map(|destination| *destination.as_uuid()),
    )
    .bind(filter_changed)
    .bind(connection_id)
    .bind(pool)
    .bind(table_id)
    .bind(threshold_changed)
    .bind(threshold)
    .bind(update.enabled)
    .bind(update.expected_revision)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("notification rule", error))?;
    match row {
        Some(row) => row.try_into(),
        None => Err(
            revision_or_missing_rule(conn, organization_id, id, update.expected_revision).await?,
        ),
    }
}

async fn revision_or_missing_rule(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: NotificationRuleId,
    expected: i64,
) -> Result<PersistenceError, PersistenceError> {
    let actual: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM notification_rules
         WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    match actual {
        Some(actual) => Ok(PersistenceError::ConfigRevisionConflict {
            entity: "notification rule",
            id: id.to_string(),
            expected,
            actual,
        }),
        None => Ok(PersistenceError::NotFound {
            entity: "notification rule",
            id: id.to_string(),
        }),
    }
}

/// One event about to be matched against the organization's rules.
#[derive(Clone, Debug)]
pub struct DeliveryEvent {
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Event kind the rules must name.
    pub event_kind: EventKind,
    /// Stable identity of the source event, unique per rule.
    pub source_key: String,
    /// When it happened.
    pub occurred_at: DateTime<Utc>,
    /// One sentence for a human.
    pub summary: String,
    /// Resource type the event is about.
    pub resource_kind: String,
    /// Durable resource identifier.
    pub resource_id: String,
    /// Human label, when one exists.
    pub resource_label: Option<String>,
    /// Stable reason code, when one exists.
    pub reason: Option<String>,
    /// Connection dimension, for a connection-filtered rule.
    pub connection_id: Option<CatalogConnectionId>,
    /// Table dimension, for a table-filtered rule.
    pub table_id: Option<TableId>,
    /// Worker-pool dimensions the event carries, for pool-filtered rules.
    pub pools: Vec<String>,
}

/// Appends one delivery per matching enabled rule, in the caller's transaction.
///
/// A rule with a repeat threshold counts distinct source keys for the event's
/// table (or resource) and fires when the count reaches the threshold; later
/// matching events keep firing until the counter has been quiet for 24 hours,
/// when it starts over. A replayed source key never advances the counter.
///
/// # Errors
///
/// Database errors.
pub async fn enqueue_event(
    conn: &mut PgConnection,
    event: &DeliveryEvent,
) -> Result<u64, PersistenceError> {
    let rules: Vec<(uuid::Uuid, Option<i32>)> = sqlx::query_as(
        "SELECT r.id, r.repeat_threshold
         FROM notification_rules r
         JOIN notification_destinations d
           ON d.organization_id = r.organization_id AND d.id = r.destination_id
         WHERE r.organization_id = $1 AND r.enabled AND d.enabled AND r.event_kind = $2
           AND (r.catalog_connection_id IS NULL
                OR r.catalog_connection_id = $3
                OR ($5::uuid IS NOT NULL AND EXISTS (
                    SELECT 1 FROM table_aliases a
                    WHERE a.organization_id = r.organization_id
                      AND a.table_identity_id = $5
                      AND a.catalog_connection_id = r.catalog_connection_id)))
           AND (r.table_identity_id IS NULL OR r.table_identity_id = $5)
           AND (r.worker_pool IS NULL OR r.worker_pool = ANY($4))
         ORDER BY r.id",
    )
    .bind(event.organization_id.as_uuid())
    .bind(event.event_kind.as_str())
    .bind(event.connection_id.map(|id| *id.as_uuid()))
    .bind(&event.pools)
    .bind(event.table_id.map(|id| *id.as_uuid()))
    .fetch_all(&mut *conn)
    .await?;
    let counter_key = event
        .table_id
        .map_or_else(|| event.resource_id.clone(), |table| table.to_string());
    let mut inserted: u64 = 0;
    for (rule_id, threshold) in rules {
        if let Some(limit) = threshold {
            // Record the source first: a replay must not advance the counter.
            let counted = sqlx::query(
                "INSERT INTO notification_rule_events
                     (organization_id, rule_id, source_key, occurred_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (organization_id, rule_id, source_key) DO NOTHING",
            )
            .bind(event.organization_id.as_uuid())
            .bind(rule_id)
            .bind(&event.source_key)
            .bind(event.occurred_at)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            if counted == 0 {
                continue;
            }
            let occurrences: i32 = sqlx::query_scalar(
                "INSERT INTO notification_rule_counters
                     (organization_id, rule_id, counter_key, occurrences, last_occurred_at)
                 VALUES ($1, $2, $3, 1, clock_timestamp())
                 ON CONFLICT (organization_id, rule_id, counter_key) DO UPDATE
                 SET occurrences = CASE
                         WHEN notification_rule_counters.last_occurred_at
                              < clock_timestamp() - interval '24 hours'
                         THEN 1
                         ELSE notification_rule_counters.occurrences + 1
                     END,
                     last_occurred_at = clock_timestamp()
                 RETURNING occurrences",
            )
            .bind(event.organization_id.as_uuid())
            .bind(rule_id)
            .bind(&counter_key)
            .fetch_one(&mut *conn)
            .await?;
            if occurrences < limit {
                continue;
            }
        }
        inserted += sqlx::query(
            "INSERT INTO notification_deliveries
                 (id, organization_id, rule_id, destination_id, event_kind,
                  source_key, occurred_at, summary, resource_kind, resource_id,
                  resource_label, reason)
             SELECT gen_random_uuid(), $1, r.id, r.destination_id, $2, $3, $4,
                    $5, $6, $7, $8, $9
             FROM notification_rules r
             WHERE r.organization_id = $1 AND r.id = $10
             ON CONFLICT (organization_id, rule_id, source_key) DO NOTHING",
        )
        .bind(event.organization_id.as_uuid())
        .bind(event.event_kind.as_str())
        .bind(&event.source_key)
        .bind(event.occurred_at)
        .bind(&event.summary)
        .bind(&event.resource_kind)
        .bind(&event.resource_id)
        .bind(event.resource_label.as_deref())
        .bind(event.reason.as_deref())
        .bind(rule_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(inserted)
}

/// One claimed delivery, joined with its destination.
#[derive(Clone, Debug)]
pub struct ClaimedDelivery {
    /// Delivery identifier, which is also the envelope identifier.
    pub id: uuid::Uuid,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Event kind.
    pub event_kind: EventKind,
    /// When the event happened.
    pub occurred_at: DateTime<Utc>,
    /// One sentence for a human.
    pub summary: String,
    /// Resource type.
    pub resource_kind: String,
    /// Resource identifier.
    pub resource_id: String,
    /// Resource label.
    pub resource_label: Option<String>,
    /// Stable reason code.
    pub reason: Option<String>,
    /// Attempts already recorded.
    pub attempts: i32,
    /// Destination identifier, for the pre-send eligibility check.
    pub destination_id: NotificationDestinationId,
    /// Destination transport shape.
    pub destination_kind: DestinationKind,
    /// Destination URL reference.
    pub url_ref: String,
    /// Signing-secret reference.
    pub signing_secret_ref: Option<String>,
    /// Whether the destination is still enabled.
    pub destination_enabled: bool,
    /// The claim this sender owns; a completion must present it.
    pub claim_token: uuid::Uuid,
}

#[derive(sqlx::FromRow)]
struct ClaimedRow {
    id: uuid::Uuid,
    organization_id: uuid::Uuid,
    event_kind: String,
    occurred_at: DateTime<Utc>,
    summary: String,
    resource_kind: String,
    resource_id: String,
    resource_label: Option<String>,
    reason: Option<String>,
    attempts: i32,
    destination_id: uuid::Uuid,
    kind: String,
    url_ref: String,
    signing_secret_ref: Option<String>,
    enabled: bool,
    claim_token: uuid::Uuid,
}

/// Claims up to `limit` due deliveries, expiring stale claims.
///
/// # Errors
///
/// Database errors, including an unknown persisted event or destination kind.
pub async fn claim_due(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<ClaimedDelivery>, PersistenceError> {
    let rows: Vec<ClaimedRow> = sqlx::query_as(
        "WITH due AS (
             SELECT d2.id
             FROM notification_deliveries d2
             WHERE d2.state = 'Pending'
               AND d2.next_attempt_at <= statement_timestamp()
               AND (d2.claimed_until IS NULL
                    OR d2.claimed_until <= statement_timestamp())
             ORDER BY d2.next_attempt_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED)
         UPDATE notification_deliveries d
         SET claimed_until = clock_timestamp() + interval '60 seconds',
             claim_token = gen_random_uuid(),
             updated_at = clock_timestamp()
         FROM notification_destinations n, due
         WHERE d.id = due.id
           AND n.organization_id = d.organization_id
           AND n.id = d.destination_id
         RETURNING d.id, d.organization_id, d.event_kind, d.occurred_at,
                   d.summary, d.resource_kind, d.resource_id, d.resource_label,
                   d.reason, d.attempts, d.destination_id, n.kind, n.url_ref,
                   n.signing_secret_ref, n.enabled, d.claim_token",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(ClaimedDelivery {
                id: row.id,
                organization_id: OrganizationId::from_uuid(row.organization_id),
                event_kind: EventKind::from_name(&row.event_kind).ok_or_else(|| {
                    PersistenceError::CorruptValue {
                        entity: "notification delivery",
                        value: format!("unknown event kind {}", row.event_kind),
                    }
                })?,
                occurred_at: row.occurred_at,
                summary: row.summary,
                resource_kind: row.resource_kind,
                resource_id: row.resource_id,
                resource_label: row.resource_label,
                reason: row.reason,
                attempts: row.attempts,
                destination_id: NotificationDestinationId::from_uuid(row.destination_id),
                destination_kind: DestinationKind::from_name(&row.kind).ok_or_else(|| {
                    PersistenceError::CorruptValue {
                        entity: "notification destination",
                        value: format!("unknown kind {}", row.kind),
                    }
                })?,
                url_ref: row.url_ref,
                signing_secret_ref: row.signing_secret_ref,
                destination_enabled: row.enabled,
                claim_token: row.claim_token,
            })
        })
        .collect()
}

/// How one send finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// A 2xx response.
    Delivered,
    /// The destination answered with a non-2xx status.
    Rejected,
    /// The request could not be completed.
    Failed,
}

impl DeliveryOutcome {
    /// The stable name persisted in PostgreSQL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "Delivered",
            Self::Rejected => "Rejected",
            Self::Failed => "Failed",
        }
    }

    /// Parses a stored outcome.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Delivered" => Some(Self::Delivered),
            "Rejected" => Some(Self::Rejected),
            "Failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One attempt's result and the delivery's next state.
pub struct DeliveryReport<'a> {
    /// What the send produced.
    pub outcome: DeliveryOutcome,
    /// The HTTP status, when a response arrived.
    pub status: Option<u16>,
    /// Stable failure code, when the send failed.
    pub reason: Option<&'a str>,
    /// The delivery's next state.
    pub state: &'a str,
    /// When to retry, for a pending delivery.
    pub next_attempt_at: Option<DateTime<Utc>>,
}

/// Records one attempt and the delivery's next state, atomically.
///
/// The completion applies only while `claim_token` still owns the delivery in
/// `Pending`; a stale sender returns `Ok(false)` and changes nothing. The
/// attempt insert commits or rolls back with the delivery update.
///
/// # Errors
///
/// Database errors.
pub async fn record_delivery(
    conn: &mut PgConnection,
    id: uuid::Uuid,
    claim_token: uuid::Uuid,
    report: &DeliveryReport<'_>,
) -> Result<bool, PersistenceError> {
    let mut tx = conn.begin().await?;
    let attempt: Option<i32> = sqlx::query_scalar(
        "UPDATE notification_deliveries
         SET attempts = attempts + 1,
             state = $2,
             next_attempt_at = COALESCE($3, next_attempt_at),
             claimed_until = NULL,
             last_error = $4,
             updated_at = clock_timestamp()
         WHERE id = $1 AND claim_token = $5 AND state = 'Pending'
         RETURNING attempts",
    )
    .bind(id)
    .bind(report.state)
    .bind(report.next_attempt_at)
    .bind(report.reason)
    .bind(claim_token)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(attempt) = attempt else {
        tx.rollback().await?;
        return Ok(false);
    };
    // Only the status and a stable reason are kept: a response body can echo
    // a credential from the destination path.
    sqlx::query(
        "INSERT INTO notification_attempts
             (delivery_id, attempt, outcome, status, reason)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(attempt)
    .bind(report.outcome.as_str())
    .bind(report.status.map(i32::from))
    .bind(report.reason)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Where a delivery stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryState {
    /// Waiting for its next attempt.
    Pending,
    /// The destination accepted it.
    Delivered,
    /// The attempt cap was reached without acceptance.
    Exhausted,
}

impl DeliveryState {
    /// The stable name persisted in PostgreSQL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Delivered => "Delivered",
            Self::Exhausted => "Exhausted",
        }
    }

    /// Parses a stored state.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Pending" => Some(Self::Pending),
            "Delivered" => Some(Self::Delivered),
            "Exhausted" => Some(Self::Exhausted),
            _ => None,
        }
    }
}

/// Optional filters for the delivery log.
#[derive(Clone, Debug, Default)]
pub struct DeliveryFilter {
    /// Only this state.
    pub state: Option<DeliveryState>,
    /// Only deliveries of this rule.
    pub rule: Option<NotificationRuleId>,
    /// Only deliveries to this destination.
    pub destination: Option<NotificationDestinationId>,
}

/// One delivery as the log shows it.
#[derive(Clone, Debug)]
pub struct NotificationDelivery {
    /// Delivery identifier.
    pub id: uuid::Uuid,
    /// Rule that enqueued it.
    pub rule_id: NotificationRuleId,
    /// Destination that receives it.
    pub destination_id: NotificationDestinationId,
    /// Destination name.
    pub destination_name: String,
    /// Destination transport shape.
    pub destination_kind: DestinationKind,
    /// Event kind.
    pub event_kind: EventKind,
    /// When the event happened.
    pub occurred_at: DateTime<Utc>,
    /// One sentence for a human.
    pub summary: String,
    /// Resource type.
    pub resource_kind: String,
    /// Resource identifier.
    pub resource_id: String,
    /// Resource label.
    pub resource_label: Option<String>,
    /// Stable reason code.
    pub reason: Option<String>,
    /// Current state.
    pub state: DeliveryState,
    /// Attempts recorded so far.
    pub attempt_count: i32,
    /// When the next attempt is due.
    pub next_attempt_at: DateTime<Utc>,
    /// The last stable failure reason.
    pub last_error: Option<String>,
    /// When the row was enqueued.
    pub created_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    id: uuid::Uuid,
    rule_id: uuid::Uuid,
    destination_id: uuid::Uuid,
    destination_name: String,
    destination_kind: String,
    event_kind: String,
    occurred_at: DateTime<Utc>,
    summary: String,
    resource_kind: String,
    resource_id: String,
    resource_label: Option<String>,
    reason: Option<String>,
    state: String,
    attempts: i32,
    next_attempt_at: DateTime<Utc>,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn delivery_from_row(row: DeliveryRow) -> Result<NotificationDelivery, PersistenceError> {
    Ok(NotificationDelivery {
        id: row.id,
        rule_id: NotificationRuleId::from_uuid(row.rule_id),
        destination_id: NotificationDestinationId::from_uuid(row.destination_id),
        destination_name: row.destination_name,
        destination_kind: DestinationKind::from_name(&row.destination_kind).ok_or_else(|| {
            PersistenceError::CorruptValue {
                entity: "notification destination",
                value: format!("unknown kind {}", row.destination_kind),
            }
        })?,
        event_kind: EventKind::from_name(&row.event_kind).ok_or_else(|| {
            PersistenceError::CorruptValue {
                entity: "notification delivery",
                value: format!("unknown event kind {}", row.event_kind),
            }
        })?,
        occurred_at: row.occurred_at,
        summary: row.summary,
        resource_kind: row.resource_kind,
        resource_id: row.resource_id,
        resource_label: row.resource_label,
        reason: row.reason,
        state: DeliveryState::from_name(&row.state).ok_or_else(|| {
            PersistenceError::CorruptValue {
                entity: "notification delivery",
                value: format!("unknown state {}", row.state),
            }
        })?,
        attempt_count: row.attempts,
        next_attempt_at: row.next_attempt_at,
        last_error: row.last_error,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Reads one bounded page of deliveries, newest first.
///
/// `after` is the `(created_at, id)` of the last row on the previous page.
///
/// # Errors
///
/// Database errors, or [`PersistenceError::CorruptValue`] for a stored value
/// this release does not know.
pub async fn deliveries(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    filter: &DeliveryFilter,
    after: Option<(DateTime<Utc>, uuid::Uuid)>,
    limit: i64,
) -> Result<Vec<NotificationDelivery>, PersistenceError> {
    let rows: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT d.id, d.rule_id, d.destination_id,
                n.name AS destination_name, n.kind AS destination_kind,
                d.event_kind, d.occurred_at, d.summary, d.resource_kind,
                d.resource_id, d.resource_label, d.reason, d.state, d.attempts,
                d.next_attempt_at, d.last_error, d.created_at, d.updated_at
         FROM notification_deliveries d
         JOIN notification_destinations n
           ON n.organization_id = d.organization_id AND n.id = d.destination_id
         WHERE d.organization_id = $1
           AND ($2::text IS NULL OR d.state = $2)
           AND ($3::uuid IS NULL OR d.rule_id = $3)
           AND ($4::uuid IS NULL OR d.destination_id = $4)
           AND ($5::timestamptz IS NULL
                OR (d.created_at, d.id) < ($5, $6))
         ORDER BY d.created_at DESC, d.id DESC
         LIMIT $7",
    )
    .bind(organization_id.as_uuid())
    .bind(filter.state.map(DeliveryState::as_str))
    .bind(filter.rule.as_ref().map(NotificationRuleId::as_uuid))
    .bind(
        filter
            .destination
            .as_ref()
            .map(NotificationDestinationId::as_uuid),
    )
    .bind(after.map(|after| after.0))
    .bind(after.map(|after| after.1))
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(delivery_from_row).collect()
}

/// Reads one delivery in the organization.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when it does not exist, and database errors.
pub async fn delivery(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: uuid::Uuid,
) -> Result<NotificationDelivery, PersistenceError> {
    let row: Option<DeliveryRow> = sqlx::query_as(
        "SELECT d.id, d.rule_id, d.destination_id,
                n.name AS destination_name, n.kind AS destination_kind,
                d.event_kind, d.occurred_at, d.summary, d.resource_kind,
                d.resource_id, d.resource_label, d.reason, d.state, d.attempts,
                d.next_attempt_at, d.last_error, d.created_at, d.updated_at
         FROM notification_deliveries d
         JOIN notification_destinations n
           ON n.organization_id = d.organization_id AND n.id = d.destination_id
         WHERE d.organization_id = $1 AND d.id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(delivery_from_row)
        .transpose()?
        .ok_or_else(|| PersistenceError::NotFound {
            entity: "notification delivery",
            id: id.to_string(),
        })
}

/// One recorded send attempt.
#[derive(Clone, Debug)]
pub struct NotificationAttempt {
    /// One-based attempt number.
    pub attempt: i32,
    /// When it finished.
    pub finished_at: DateTime<Utc>,
    /// What the send produced.
    pub outcome: DeliveryOutcome,
    /// The HTTP status, when a response arrived.
    pub status: Option<i32>,
    /// Stable failure reason.
    pub reason: Option<String>,
}

#[derive(sqlx::FromRow)]
struct AttemptRow {
    attempt: i32,
    finished_at: DateTime<Utc>,
    outcome: String,
    status: Option<i32>,
    reason: Option<String>,
}

/// Reads a delivery's attempts in order.
///
/// # Errors
///
/// Database errors, or [`PersistenceError::CorruptValue`] for an outcome this
/// release does not know.
pub async fn attempts(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: uuid::Uuid,
) -> Result<Vec<NotificationAttempt>, PersistenceError> {
    let rows: Vec<AttemptRow> = sqlx::query_as(
        "SELECT a.attempt, a.finished_at, a.outcome, a.status, a.reason
         FROM notification_attempts a
         JOIN notification_deliveries d ON d.id = a.delivery_id
         WHERE d.organization_id = $1 AND d.id = $2
         ORDER BY a.attempt",
    )
    .bind(organization_id.as_uuid())
    .bind(id)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(NotificationAttempt {
                attempt: row.attempt,
                finished_at: row.finished_at,
                outcome: DeliveryOutcome::from_name(&row.outcome).ok_or_else(|| {
                    PersistenceError::CorruptValue {
                        entity: "notification attempt",
                        value: format!("unknown outcome {}", row.outcome),
                    }
                })?,
                status: row.status,
                reason: row.reason,
            })
        })
        .collect()
}

/// Deletes delivered and exhausted deliveries older than `before`.
///
/// # Errors
///
/// Database errors.
pub async fn prune_deliveries(
    conn: &mut PgConnection,
    before: DateTime<Utc>,
) -> Result<u64, PersistenceError> {
    let deleted = sqlx::query(
        "DELETE FROM notification_deliveries
         WHERE state IN ('Delivered', 'Exhausted') AND updated_at < $1",
    )
    .bind(before)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query("DELETE FROM notification_rule_events WHERE occurred_at < $1")
        .bind(before)
        .execute(&mut *conn)
        .await?;
    Ok(deleted)
}

/// A commit outcome still unknown past its connection's bound.
#[derive(Clone, Debug)]
pub struct OverdueUnknown {
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Job whose outcome is unknown.
    pub job_id: JobId,
    /// Table the job maintains.
    pub table_id: TableId,
    /// Connection whose bound passed.
    pub connection_id: CatalogConnectionId,
    /// Dispatch pools of the job's tasks, for pool-filtered rules.
    pub pools: Vec<String>,
}

/// Finds unknown commit outcomes older than their connection's bound, in
/// deterministic order.
///
/// # Errors
///
/// Database errors.
pub async fn overdue_unknown_publications(
    conn: &mut PgConnection,
) -> Result<Vec<OverdueUnknown>, PersistenceError> {
    let rows: Vec<(uuid::Uuid, uuid::Uuid, uuid::Uuid, uuid::Uuid, Vec<String>)> = sqlx::query_as(
        "SELECT j.organization_id, j.id, j.table_identity_id,
                    (i.content->>'catalog_connection_id')::uuid,
                    COALESCE(array_agg(DISTINCT ds.pool)
                             FILTER (WHERE ds.pool IS NOT NULL), ARRAY[]::text[])
             FROM jobs j
             JOIN publication_intents i ON i.job_id = j.id
             JOIN catalog_connections c
               ON c.organization_id = j.organization_id
              AND c.id = (i.content->>'catalog_connection_id')::uuid
             LEFT JOIN tasks t ON t.job_id = j.id
             LEFT JOIN dispatch_specs ds ON ds.task_id = t.id
             WHERE j.state = 'CommitOutcomeUnknown'
               AND j.publication_started_at IS NOT NULL
               AND j.publication_started_at
                   < clock_timestamp()
                     - make_interval(secs => c.max_unresolved_publication_seconds)
             GROUP BY j.organization_id, j.id, j.table_identity_id,
                      (i.content->>'catalog_connection_id')
             ORDER BY j.organization_id, j.id",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(organization_id, job_id, table_id, connection_id, pools)| OverdueUnknown {
                organization_id: OrganizationId::from_uuid(organization_id),
                job_id: JobId::from_uuid(job_id),
                table_id: TableId::from_uuid(table_id),
                connection_id: CatalogConnectionId::from_uuid(connection_id),
                pools,
            },
        )
        .collect())
}

/// A worker pool with pending work and no live session.
#[derive(Clone, Debug)]
pub struct PoolGap {
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Pool name.
    pub pool: String,
}

/// Finds pools that have pending tasks and no live, non-draining session.
///
/// # Errors
///
/// Database errors.
pub async fn pools_without_workers(
    conn: &mut PgConnection,
) -> Result<Vec<PoolGap>, PersistenceError> {
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT j.organization_id, ds.pool
         FROM tasks t
         JOIN jobs j ON j.id = t.job_id
         JOIN dispatch_specs ds ON ds.task_id = t.id
         WHERE t.state IN ('Pending', 'RetryWaiting')
           AND j.state NOT IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
           AND NOT EXISTS (
               SELECT 1 FROM worker_sessions w
               WHERE w.organization_id = j.organization_id
                 AND w.pool = ds.pool
                 AND w.lease_expires_at > clock_timestamp()
                 AND NOT w.draining
                 AND NOT w.superseded)
         ORDER BY j.organization_id, ds.pool",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(organization_id, pool)| PoolGap {
            organization_id: OrganizationId::from_uuid(organization_id),
            pool,
        })
        .collect())
}
