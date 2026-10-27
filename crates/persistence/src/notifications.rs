//! Notification destinations (BS-029.T1, ADR 0060).
//!
//! A destination is an organization-scoped webhook or Slack-compatible
//! endpoint. It holds a URL reference and an optional signing-secret reference,
//! plus an optimistic `revision`, so two editors cannot silently overwrite each
//! other. The URL policy (HTTPS plus the deployment allow-list) is validated on
//! the resolved URL before it reaches this module; both stored fields are names,
//! never values.

use bergsmith_common::ids::{
    CatalogConnectionId, NotificationDestinationId, NotificationRuleId, OrganizationId,
    PrincipalId, TableId,
};
use bergsmith_common::notifications::{DestinationKind, EventKind};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;

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
