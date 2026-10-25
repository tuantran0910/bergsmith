//! Metadata-only table samples and their bounded history (BS-025.T2).
//!
//! A sample is an observation, not a cache: it is append-only, keyed by when
//! it was taken, and pruned by age. Callers decide when to take one; this
//! module never talks to a catalog.

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::PersistenceError;

/// The counter columns, in the order every query uses them.
macro_rules! counters {
    () => {
        "record_count, data_files, data_bytes, delete_files, delete_bytes, \
         position_delete_files, equality_delete_files, position_delete_records, \
         equality_delete_records, manifest_count, manifest_bytes, partition_count, \
         small_files, target_file_size_bytes, snapshot_count"
    };
}

/// The columns of one stored sample, in row order.
macro_rules! sample_columns {
    () => {
        concat!(
            "metadata_location, snapshot_id, last_operation, ",
            counters!(),
            ", oldest_snapshot_at, newest_snapshot_at, size_histogram, top_partitions"
        )
    };
}

/// One sample to append.
///
/// Counters are `u64` because that is what metadata records; the repository
/// refuses a value that does not fit the ledger's `bigint` instead of
/// truncating it. The histogram and partition list are stored as the JSON
/// arrays their producer serialized, so ordering is preserved.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTableSample {
    /// The connection the table was read through.
    pub catalog_connection_id: CatalogConnectionId,
    /// When the sample was taken.
    pub sampled_at: DateTime<Utc>,
    /// The `metadata.json` the sample describes.
    pub metadata_location: String,
    /// The current snapshot.
    pub snapshot_id: Option<i64>,
    /// The current snapshot's operation.
    pub last_operation: Option<String>,
    /// Rows in live data files.
    pub record_count: u64,
    /// Live data files.
    pub data_files: u64,
    /// Bytes of live data files.
    pub data_bytes: u64,
    /// Live delete files.
    pub delete_files: u64,
    /// Bytes of live delete files.
    pub delete_bytes: u64,
    /// Live position delete files.
    pub position_delete_files: u64,
    /// Live equality delete files.
    pub equality_delete_files: u64,
    /// Rows recorded by live position delete files.
    pub position_delete_records: u64,
    /// Rows recorded by live equality delete files.
    pub equality_delete_records: u64,
    /// Manifests in the current snapshot's manifest list.
    pub manifest_count: u64,
    /// Bytes of those manifests.
    pub manifest_bytes: u64,
    /// Distinct partitions among live data files.
    pub partition_count: u64,
    /// Data files below the target size.
    pub small_files: u64,
    /// The target size small files are measured against.
    pub target_file_size_bytes: u64,
    /// Snapshots the table retained.
    pub snapshot_count: u64,
    /// The oldest entry in the snapshot log.
    pub oldest_snapshot_at: Option<DateTime<Utc>>,
    /// The newest entry in the snapshot log.
    pub newest_snapshot_at: Option<DateTime<Utc>>,
    /// Data-file size buckets, as produced by the collector.
    pub size_histogram: Value,
    /// The busiest partitions, as produced by the collector.
    pub top_partitions: Value,
}

/// One stored sample.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTableSample {
    /// When the sample was taken.
    pub sampled_at: DateTime<Utc>,
    /// The connection the table was read through.
    pub catalog_connection_id: CatalogConnectionId,
    /// The `metadata.json` the sample describes.
    pub metadata_location: String,
    /// The current snapshot.
    pub snapshot_id: Option<i64>,
    /// The current snapshot's operation.
    pub last_operation: Option<String>,
    /// Rows in live data files.
    pub record_count: i64,
    /// Live data files.
    pub data_files: i64,
    /// Bytes of live data files.
    pub data_bytes: i64,
    /// Live delete files.
    pub delete_files: i64,
    /// Bytes of live delete files.
    pub delete_bytes: i64,
    /// Live position delete files.
    pub position_delete_files: i64,
    /// Live equality delete files.
    pub equality_delete_files: i64,
    /// Rows recorded by live position delete files.
    pub position_delete_records: i64,
    /// Rows recorded by live equality delete files.
    pub equality_delete_records: i64,
    /// Manifests in the current snapshot's manifest list.
    pub manifest_count: i64,
    /// Bytes of those manifests.
    pub manifest_bytes: i64,
    /// Distinct partitions among live data files.
    pub partition_count: i64,
    /// Data files below the target size.
    pub small_files: i64,
    /// The target size small files are measured against.
    pub target_file_size_bytes: i64,
    /// Snapshots the table retained.
    pub snapshot_count: i64,
    /// The oldest entry in the snapshot log.
    pub oldest_snapshot_at: Option<DateTime<Utc>>,
    /// The newest entry in the snapshot log.
    pub newest_snapshot_at: Option<DateTime<Utc>>,
    /// Data-file size buckets.
    pub size_histogram: Value,
    /// The busiest partitions.
    pub top_partitions: Value,
}

/// A table whose latest sample is older than the sampling interval.
#[derive(Clone, Debug, PartialEq)]
pub struct DueTable {
    /// The owning organization.
    pub organization_id: OrganizationId,
    /// The canonical table.
    pub table_id: TableId,
    /// The table's Iceberg `table-uuid`, checked against the loaded table.
    pub table_uuid: Uuid,
    /// When the latest sample was taken, or `None` if never.
    pub last_sampled_at: Option<DateTime<Utc>>,
}

/// One enabled alias of a table that may run maintenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceRoute {
    /// The connection the alias belongs to.
    pub connection_id: CatalogConnectionId,
    /// The alias's namespace levels.
    pub namespace: Vec<String>,
    /// The alias's table name.
    pub table_name: String,
}

/// Totals over the newest sample of every visible table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatisticsTotals {
    /// Tables with at least one sample.
    pub tables: i64,
    /// Rows in live data files.
    pub record_count: i64,
    /// Live data files.
    pub data_files: i64,
    /// Bytes of live data files.
    pub data_bytes: i64,
    /// Live delete files.
    pub delete_files: i64,
    /// Bytes of live delete files.
    pub delete_bytes: i64,
    /// Data files below the target size.
    pub small_files: i64,
    /// Snapshots retained across those tables.
    pub snapshot_count: i64,
    /// The newest sample of any of those tables.
    pub last_sampled_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct SampleRow {
    sampled_at: DateTime<Utc>,
    catalog_connection_id: Uuid,
    metadata_location: String,
    snapshot_id: Option<i64>,
    last_operation: Option<String>,
    record_count: i64,
    data_files: i64,
    data_bytes: i64,
    delete_files: i64,
    delete_bytes: i64,
    position_delete_files: i64,
    equality_delete_files: i64,
    position_delete_records: i64,
    equality_delete_records: i64,
    manifest_count: i64,
    manifest_bytes: i64,
    partition_count: i64,
    small_files: i64,
    target_file_size_bytes: i64,
    snapshot_count: i64,
    oldest_snapshot_at: Option<DateTime<Utc>>,
    newest_snapshot_at: Option<DateTime<Utc>>,
    size_histogram: Value,
    top_partitions: Value,
}

impl From<SampleRow> for StoredTableSample {
    fn from(row: SampleRow) -> Self {
        Self {
            sampled_at: row.sampled_at,
            catalog_connection_id: CatalogConnectionId::from_uuid(row.catalog_connection_id),
            metadata_location: row.metadata_location,
            snapshot_id: row.snapshot_id,
            last_operation: row.last_operation,
            record_count: row.record_count,
            data_files: row.data_files,
            data_bytes: row.data_bytes,
            delete_files: row.delete_files,
            delete_bytes: row.delete_bytes,
            position_delete_files: row.position_delete_files,
            equality_delete_files: row.equality_delete_files,
            position_delete_records: row.position_delete_records,
            equality_delete_records: row.equality_delete_records,
            manifest_count: row.manifest_count,
            manifest_bytes: row.manifest_bytes,
            partition_count: row.partition_count,
            small_files: row.small_files,
            target_file_size_bytes: row.target_file_size_bytes,
            snapshot_count: row.snapshot_count,
            oldest_snapshot_at: row.oldest_snapshot_at,
            newest_snapshot_at: row.newest_snapshot_at,
            size_histogram: row.size_histogram,
            top_partitions: row.top_partitions,
        }
    }
}

/// A counter that fits the ledger's `bigint`.
fn counter(entity: &'static str, value: u64) -> Result<i64, PersistenceError> {
    i64::try_from(value).map_err(|_| PersistenceError::InvalidValue {
        entity,
        constraint: "counter_fits_bigint".to_owned(),
    })
}

/// Appends one sample. A sample already recorded at the same instant is not
/// duplicated.
///
/// Returns whether a row was inserted.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when the table or connection is not
/// in the organization, and [`PersistenceError::InvalidValue`] when a counter
/// does not fit a `bigint` or a value fails a check.
pub async fn record(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    sample: &NewTableSample,
) -> Result<bool, PersistenceError> {
    const ENTITY: &str = "table statistics";
    let result = sqlx::query(concat!(
        "INSERT INTO table_statistics (organization_id, table_identity_id, \
         catalog_connection_id, sampled_at, ",
        sample_columns!(),
        ") VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
         $17, $18, $19, $20, $21, $22, $23, $24, $25, $26) \
         ON CONFLICT (organization_id, table_identity_id, sampled_at) DO NOTHING"
    ))
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(sample.catalog_connection_id.as_uuid())
    .bind(sample.sampled_at)
    .bind(&sample.metadata_location)
    .bind(sample.snapshot_id)
    .bind(sample.last_operation.as_deref())
    .bind(counter(ENTITY, sample.record_count)?)
    .bind(counter(ENTITY, sample.data_files)?)
    .bind(counter(ENTITY, sample.data_bytes)?)
    .bind(counter(ENTITY, sample.delete_files)?)
    .bind(counter(ENTITY, sample.delete_bytes)?)
    .bind(counter(ENTITY, sample.position_delete_files)?)
    .bind(counter(ENTITY, sample.equality_delete_files)?)
    .bind(counter(ENTITY, sample.position_delete_records)?)
    .bind(counter(ENTITY, sample.equality_delete_records)?)
    .bind(counter(ENTITY, sample.manifest_count)?)
    .bind(counter(ENTITY, sample.manifest_bytes)?)
    .bind(counter(ENTITY, sample.partition_count)?)
    .bind(counter(ENTITY, sample.small_files)?)
    .bind(counter(ENTITY, sample.target_file_size_bytes)?)
    .bind(counter(ENTITY, sample.snapshot_count)?)
    .bind(sample.oldest_snapshot_at)
    .bind(sample.newest_snapshot_at)
    .bind(&sample.size_histogram)
    .bind(&sample.top_partitions)
    .execute(conn)
    .await
    .map_err(|error| PersistenceError::from_write(ENTITY, error))?;
    Ok(result.rows_affected() == 1)
}

/// The newest sample of `table_id`, if it has been sampled.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn latest(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
) -> Result<Option<StoredTableSample>, PersistenceError> {
    let row: Option<SampleRow> = sqlx::query_as(concat!(
        "SELECT sampled_at, catalog_connection_id, ",
        sample_columns!(),
        " FROM table_statistics WHERE organization_id = $1 AND table_identity_id = $2 \
         ORDER BY sampled_at DESC LIMIT 1"
    ))
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(row.map(StoredTableSample::from))
}

/// Samples of `table_id` newest first, starting strictly before `before` and
/// at or after `since`.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn history(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    before: Option<DateTime<Utc>>,
    since: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<StoredTableSample>, PersistenceError> {
    let rows: Vec<SampleRow> = sqlx::query_as(concat!(
        "SELECT sampled_at, catalog_connection_id, ",
        sample_columns!(),
        " FROM table_statistics WHERE organization_id = $1 AND table_identity_id = $2 \
         AND ($3::timestamptz IS NULL OR sampled_at < $3) \
         AND ($4::timestamptz IS NULL OR sampled_at >= $4) \
         ORDER BY sampled_at DESC LIMIT $5"
    ))
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(before)
    .bind(since)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(StoredTableSample::from).collect())
}

/// Tables whose latest sample is at or before `cutoff`, oldest first.
///
#[derive(sqlx::FromRow)]
struct DueRow {
    organization_id: Uuid,
    table_id: Uuid,
    table_uuid: Uuid,
    last_sampled_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    connection_id: Uuid,
    namespace: Vec<String>,
    table_name: String,
}

/// A table is a candidate only when some alias is on an enabled connection
/// that may run maintenance. The collector then asks for the usable routes.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn due_tables(
    conn: &mut PgConnection,
    cutoff: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<DueTable>, PersistenceError> {
    let rows: Vec<DueRow> = sqlx::query_as(
        "SELECT t.organization_id, t.id AS table_id, t.table_uuid, \
                    latest.sampled_at AS last_sampled_at \
             FROM table_identities t \
             JOIN LATERAL ( \
                 SELECT 1 FROM table_aliases a \
                 JOIN catalog_connections c \
                   ON c.organization_id = a.organization_id AND c.id = a.catalog_connection_id \
                 WHERE a.organization_id = t.organization_id AND a.table_identity_id = t.id \
                   AND c.enabled AND c.maintenance_enabled \
                 LIMIT 1 \
             ) route ON true \
             LEFT JOIN LATERAL ( \
                 SELECT s.sampled_at FROM table_statistics s \
                 WHERE s.organization_id = t.organization_id AND s.table_identity_id = t.id \
                 ORDER BY s.sampled_at DESC LIMIT 1 \
             ) latest ON true \
             WHERE latest.sampled_at IS NULL OR latest.sampled_at <= $1 \
             ORDER BY latest.sampled_at ASC NULLS FIRST, t.id \
             LIMIT $2",
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| DueTable {
            organization_id: OrganizationId::from_uuid(row.organization_id),
            table_id: TableId::from_uuid(row.table_id),
            table_uuid: row.table_uuid,
            last_sampled_at: row.last_sampled_at,
        })
        .collect())
}

/// The aliases of `table_id` that may run maintenance, lowest connection first.
///
/// A table can be reachable through several catalogs. The collector prefers the
/// first route and falls back to the next when one cannot be read, so a stale
/// or unreachable alias does not stop that table's statistics.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn maintenance_routes(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
) -> Result<Vec<MaintenanceRoute>, PersistenceError> {
    let rows: Vec<RouteRow> = sqlx::query_as(
        "SELECT a.catalog_connection_id AS connection_id, a.namespace, a.table_name \
         FROM table_aliases a \
         JOIN catalog_connections c \
           ON c.organization_id = a.organization_id AND c.id = a.catalog_connection_id \
         WHERE a.organization_id = $1 AND a.table_identity_id = $2 \
           AND c.enabled AND c.maintenance_enabled \
         ORDER BY a.catalog_connection_id, a.namespace, a.table_name",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| MaintenanceRoute {
            connection_id: CatalogConnectionId::from_uuid(row.connection_id),
            namespace: row.namespace,
            table_name: row.table_name,
        })
        .collect())
}

/// Deletes at most `batch` samples older than `older_than`, oldest first.
///
/// Returns how many rows were deleted.
///
/// # Errors
///
/// [`PersistenceError::InvalidValue`] when the batch is not positive, and
/// [`PersistenceError::Database`] when the query fails.
pub async fn prune(
    conn: &mut PgConnection,
    older_than: DateTime<Utc>,
    batch: i64,
) -> Result<u64, PersistenceError> {
    if batch <= 0 {
        return Err(PersistenceError::InvalidValue {
            entity: "table statistics",
            constraint: "prune_batch_positive".to_owned(),
        });
    }
    let result = sqlx::query(
        "DELETE FROM table_statistics WHERE ctid IN ( \
             SELECT ctid FROM table_statistics WHERE sampled_at < $1 \
             ORDER BY sampled_at LIMIT $2)",
    )
    .bind(older_than)
    .bind(batch)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

#[derive(sqlx::FromRow)]
struct TotalsRow {
    tables: i64,
    record_count: i64,
    data_files: i64,
    data_bytes: i64,
    delete_files: i64,
    delete_bytes: i64,
    small_files: i64,
    snapshot_count: i64,
    last_sampled_at: Option<DateTime<Utc>>,
}

/// Totals over the newest sample of every table the caller may see, in the
/// whole organization or on one connection.
///
/// The visibility predicate is the one the inventory API uses: an
/// organization-wide, table-scoped, or connection-scoped grant of one of
/// `roles`.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn totals(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[String],
    connection_id: Option<CatalogConnectionId>,
) -> Result<StatisticsTotals, PersistenceError> {
    let row: TotalsRow = sqlx::query_as(
        "WITH visible AS ( \
             SELECT t.id FROM table_identities t \
             WHERE t.organization_id = $1 AND EXISTS ( \
                 SELECT 1 FROM principal_role_grants g \
                 WHERE g.organization_id = $1 AND g.principal_id = $2 \
                   AND g.revoked_at IS NULL AND g.role = ANY($3::text[]) \
                   AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL) \
                     OR g.table_identity_id = t.id \
                     OR EXISTS (SELECT 1 FROM table_aliases a \
                                WHERE a.organization_id = $1 AND a.table_identity_id = t.id \
                                  AND a.catalog_connection_id = g.catalog_connection_id))) \
               AND ($4::uuid IS NULL OR EXISTS ( \
                 SELECT 1 FROM table_aliases a \
                 WHERE a.organization_id = $1 AND a.table_identity_id = t.id \
                   AND a.catalog_connection_id = $4)) \
         ), latest AS ( \
             SELECT DISTINCT ON (s.table_identity_id) s.* \
             FROM table_statistics s \
             JOIN visible v ON v.id = s.table_identity_id \
             WHERE s.organization_id = $1 \
             ORDER BY s.table_identity_id, s.sampled_at DESC \
         ) \
         SELECT count(*) AS tables, \
                COALESCE(sum(record_count), 0)::bigint AS record_count, \
                COALESCE(sum(data_files), 0)::bigint AS data_files, \
                COALESCE(sum(data_bytes), 0)::bigint AS data_bytes, \
                COALESCE(sum(delete_files), 0)::bigint AS delete_files, \
                COALESCE(sum(delete_bytes), 0)::bigint AS delete_bytes, \
                COALESCE(sum(small_files), 0)::bigint AS small_files, \
                COALESCE(sum(snapshot_count), 0)::bigint AS snapshot_count, \
                max(sampled_at) AS last_sampled_at \
         FROM latest",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(roles)
    .bind(connection_id.map(|id| *id.as_uuid()))
    .fetch_one(conn)
    .await?;
    Ok(StatisticsTotals {
        tables: row.tables,
        record_count: row.record_count,
        data_files: row.data_files,
        data_bytes: row.data_bytes,
        delete_files: row.delete_files,
        delete_bytes: row.delete_bytes,
        small_files: row.small_files,
        snapshot_count: row.snapshot_count,
        last_sampled_at: row.last_sampled_at,
    })
}
