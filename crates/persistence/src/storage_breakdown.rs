//! Read-only storage observations; failed checks never replace a complete sample.

use crate::{PersistenceError, statistics::DueTable};
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

/// Latest complete observation with its independent most recent check.
#[derive(Debug, sqlx::FromRow)]
pub struct StoredStorage {
    /// When the last complete scan finished.
    pub sampled_at: Option<DateTime<Utc>>,
    /// Main snapshot at that scan, absent for an empty table.
    pub snapshot_id: Option<i64>,
    /// Versioned numeric accounting document; never contains paths.
    pub content: Option<Value>,
    /// Most recent attempted scan.
    pub checked_at: DateTime<Utc>,
    /// Stable failure code; no third-party error strings.
    pub failure_reason: Option<String>,
}

/// Reads the org-scoped latest observation.
///
/// # Errors
/// Returns a database error if the read fails.
pub async fn latest(
    conn: &mut PgConnection,
    org: OrganizationId,
    table: TableId,
) -> Result<Option<StoredStorage>, PersistenceError> {
    Ok(sqlx::query_as("SELECT sampled_at, snapshot_id, content, checked_at, failure_reason FROM table_storage WHERE organization_id=$1 AND table_identity_id=$2")
        .bind(org.as_uuid()).bind(table.as_uuid()).fetch_optional(conn).await?)
}

/// One complete scan, including its internal baseline identity.
pub struct Observation<'a> {
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Canonical table identity.
    pub table_id: TableId,
    /// Catalog metadata location, internal only.
    pub metadata_location: &'a str,
    /// Current main snapshot, if any.
    pub snapshot_id: Option<i64>,
    /// Complete versioned counts.
    pub content: &'a Value,
    /// Scan completion time.
    pub at: DateTime<Utc>,
}

/// Records success in a short leadership-fenced transaction.
///
/// # Errors
/// Returns leadership or database errors; partial writes roll back.
pub async fn record(
    conn: &mut PgConnection,
    epoch: i64,
    sample: &Observation<'_>,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    sqlx::query("INSERT INTO table_storage (organization_id,table_identity_id,metadata_location,snapshot_id,content,sampled_at,checked_at) VALUES ($1,$2,$3,$4,$5,$6,$6) ON CONFLICT (organization_id,table_identity_id) DO UPDATE SET metadata_location=$3,snapshot_id=$4,content=$5,sampled_at=$6,checked_at=$6,failure_reason=NULL")
        .bind(sample.organization_id.as_uuid()).bind(sample.table_id.as_uuid()).bind(sample.metadata_location)
        .bind(sample.snapshot_id).bind(sample.content).bind(sample.at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Records a failed check without erasing the previous complete observation.
///
/// # Errors
/// Returns leadership or database errors.
pub async fn failure(
    conn: &mut PgConnection,
    epoch: i64,
    org: OrganizationId,
    table: TableId,
    at: DateTime<Utc>,
    reason: &str,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    sqlx::query("INSERT INTO table_storage (organization_id,table_identity_id,checked_at,failure_reason) VALUES ($1,$2,$3,$4) ON CONFLICT (organization_id,table_identity_id) DO UPDATE SET checked_at=$3,failure_reason=$4")
        .bind(org.as_uuid()).bind(table.as_uuid()).bind(at).bind(reason).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Selects due registered tables; failures wait the same interval as successes.
///
/// # Errors
/// Returns a database error if selection fails.
pub async fn due(
    conn: &mut PgConnection,
    before: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<DueTable>, PersistenceError> {
    let rows: Vec<(Uuid,Uuid,Uuid,Option<DateTime<Utc>>)> = sqlx::query_as("SELECT t.organization_id,t.id,t.table_uuid,s.checked_at FROM table_identities t LEFT JOIN table_storage s ON s.organization_id=t.organization_id AND s.table_identity_id=t.id WHERE (s.checked_at IS NULL OR s.checked_at < $1) AND EXISTS (SELECT 1 FROM table_aliases a JOIN catalog_connections c ON c.organization_id=a.organization_id AND c.id=a.catalog_connection_id WHERE a.organization_id=t.organization_id AND a.table_identity_id=t.id AND c.enabled) ORDER BY s.checked_at NULLS FIRST,t.organization_id,t.id LIMIT $2")
        .bind(before).bind(limit).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|(o, t, u, at)| DueTable {
            organization_id: OrganizationId::from_uuid(o),
            table_id: TableId::from_uuid(t),
            table_uuid: u,
            last_confirmed_at: at,
        })
        .collect())
}

/// Visible catalog totals, deduplicated by table identity (not physical object).
///
/// # Errors
/// Returns a database error if aggregation fails.
pub async fn totals(
    conn: &mut PgConnection,
    org: OrganizationId,
    principal: PrincipalId,
    roles: &[String],
    catalog: CatalogConnectionId,
    fresh_after: DateTime<Utc>,
) -> Result<Value, PersistenceError> {
    Ok(sqlx::query_scalar(concat!(
        "WITH visible AS (SELECT t.id FROM table_identities t WHERE t.organization_id=$1 AND ",
        crate::grants::table_visible!(),
        r#" AND EXISTS (
                SELECT 1 FROM table_aliases a WHERE a.organization_id=$1
                  AND a.table_identity_id=t.id AND a.catalog_connection_id=$4))
            SELECT jsonb_build_object(
                'tables', count(*)::text,
                'assessed', count(s.sampled_at)::text,
                'fresh', count(*) FILTER (WHERE s.sampled_at >= $5 AND s.failure_reason IS NULL)::text,
                'failed', count(s.failure_reason)::text,
                'stale', count(*) FILTER (WHERE s.sampled_at IS NOT NULL
                    AND (s.sampled_at < $5 OR s.failure_reason IS NOT NULL))::text,
                'oldest_sampled_at', min(s.sampled_at),
                'newest_sampled_at', max(s.sampled_at),
                'live', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'live'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'live'->>'bytes')::numeric),0)::text),
                'retained', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'retained'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'retained'->>'bytes')::numeric),0)::text),
                'metadata', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'metadata'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'metadata'->>'bytes')::numeric),0)::text),
                'orphan', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'orphan'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'orphan'->>'bytes')::numeric),0)::text),
                'protected', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'protected'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'protected'->>'bytes')::numeric),0)::text),
                'total', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'total'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'total'->>'bytes')::numeric),0)::text),
                'external', jsonb_build_object(
                    'files', COALESCE(sum((s.content->'external'->>'files')::numeric),0)::text,
                    'bytes', COALESCE(sum((s.content->'external'->>'bytes')::numeric),0)::text))
            FROM visible v LEFT JOIN table_storage s
              ON s.organization_id=$1 AND s.table_identity_id=v.id"#,
    ))
    .bind(org.as_uuid()).bind(principal.as_uuid()).bind(roles)
    .bind(catalog.as_uuid()).bind(fresh_after).fetch_one(conn).await?)
}

/// Enabled aliases usable for read-only reporting, including paused maintenance.
///
/// # Errors
/// Returns a database error if resolution fails.
pub async fn routes(
    conn: &mut PgConnection,
    org: OrganizationId,
    table: TableId,
) -> Result<Vec<crate::statistics::MaintenanceRoute>, PersistenceError> {
    let rows: Vec<(Uuid,Vec<String>,String)> = sqlx::query_as("SELECT a.catalog_connection_id,a.namespace,a.table_name FROM table_aliases a JOIN catalog_connections c ON c.organization_id=a.organization_id AND c.id=a.catalog_connection_id WHERE a.organization_id=$1 AND a.table_identity_id=$2 AND c.enabled ORDER BY a.catalog_connection_id,a.namespace,a.table_name LIMIT 3")
        .bind(org.as_uuid()).bind(table.as_uuid()).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, namespace, table_name)| crate::statistics::MaintenanceRoute {
                connection_id: CatalogConnectionId::from_uuid(id),
                namespace,
                table_name,
            },
        )
        .collect())
}
