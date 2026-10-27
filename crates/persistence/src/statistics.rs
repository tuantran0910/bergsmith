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
         equality_delete_records, manifest_count, data_manifest_count, manifest_bytes, \
         partition_count, max_partition_files, small_files, target_file_size_bytes, \
         snapshot_count"
    };
}

/// The counters a sample written by this build carries.
///
/// A sample below this version predates some counter columns, which are zero
/// for it; the collector replaces it before taking the unchanged shortcut.
pub const SAMPLE_VERSION: i16 = 1;

/// The columns of one stored sample, in row order.
macro_rules! sample_columns {
    () => {
        concat!(
            "metadata_location, snapshot_id, last_operation, sample_version, ",
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
    /// Manifests in the current snapshot's manifest list, of either kind.
    pub manifest_count: u64,
    /// Manifests that carry data files, the fragmentation denominator.
    pub data_manifest_count: u64,
    /// Bytes of those manifests.
    pub manifest_bytes: u64,
    /// Distinct partitions among live data files.
    pub partition_count: u64,
    /// The busiest partition's data-file count, independent of the display
    /// bound.
    pub max_partition_files: u64,
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
    /// Which counters the sample carries; [`SAMPLE_VERSION`] when current.
    pub sample_version: i16,
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
    /// Manifests in the current snapshot's manifest list, of either kind.
    pub manifest_count: i64,
    /// Manifests that carry data files.
    pub data_manifest_count: i64,
    /// Bytes of those manifests.
    pub manifest_bytes: i64,
    /// Distinct partitions among live data files.
    pub partition_count: i64,
    /// The busiest partition's data-file count.
    pub max_partition_files: i64,
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

/// A table whose latest confirmation is older than the sampling interval.
#[derive(Clone, Debug, PartialEq)]
pub struct DueTable {
    /// The owning organization.
    pub organization_id: OrganizationId,
    /// The canonical table.
    pub table_id: TableId,
    /// The table's Iceberg `table-uuid`, checked against the loaded table.
    pub table_uuid: Uuid,
    /// When metadata was last confirmed, or `None` if never checked. An
    /// unchanged table is confirmed on every pass without a new sample.
    pub last_confirmed_at: Option<DateTime<Utc>>,
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
struct TableSampleRow {
    table_identity_id: Uuid,
    #[sqlx(flatten)]
    sample: SampleRow,
}

#[derive(sqlx::FromRow)]
struct SampleRow {
    sampled_at: DateTime<Utc>,
    sample_version: i16,
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
    data_manifest_count: i64,
    manifest_bytes: i64,
    partition_count: i64,
    max_partition_files: i64,
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
            sample_version: row.sample_version,
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
            data_manifest_count: row.data_manifest_count,
            manifest_bytes: row.manifest_bytes,
            partition_count: row.partition_count,
            max_partition_files: row.max_partition_files,
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
         $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29) \
         ON CONFLICT (organization_id, table_identity_id, sampled_at) DO NOTHING"
    ))
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(sample.catalog_connection_id.as_uuid())
    .bind(sample.sampled_at)
    .bind(&sample.metadata_location)
    .bind(sample.snapshot_id)
    .bind(sample.last_operation.as_deref())
    .bind(SAMPLE_VERSION)
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
    .bind(counter(ENTITY, sample.data_manifest_count)?)
    .bind(counter(ENTITY, sample.manifest_bytes)?)
    .bind(counter(ENTITY, sample.partition_count)?)
    .bind(counter(ENTITY, sample.max_partition_files)?)
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

/// Records a successful metadata read, even when it produced no new sample.
///
/// The newest confirmation wins: a late or retried call never moves it back.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when the table is not in the
/// organization, and [`PersistenceError::Database`] when the write fails.
pub async fn record_check(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    checked_at: DateTime<Utc>,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "INSERT INTO table_statistics_checks (organization_id, table_identity_id, checked_at) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (organization_id, table_identity_id) DO UPDATE \
         SET checked_at = GREATEST(table_statistics_checks.checked_at, EXCLUDED.checked_at)",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(checked_at)
    .execute(conn)
    .await
    .map_err(|error| PersistenceError::from_write("table statistics check", error))?;
    Ok(())
}

/// When a pass last confirmed the table's metadata, for freshness.
///
/// The confirmation is the newer of the recorded check and the newest sample,
/// so tables sampled before confirmations were recorded still have a defensible
/// instant.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn last_confirmed_at(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
) -> Result<Option<DateTime<Utc>>, PersistenceError> {
    let confirmed: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT GREATEST( \
             (SELECT checked_at FROM table_statistics_checks \
              WHERE organization_id = $1 AND table_identity_id = $2), \
             (SELECT max(sampled_at) FROM table_statistics \
              WHERE organization_id = $1 AND table_identity_id = $2))",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .fetch_one(conn)
    .await?;
    Ok(confirmed)
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

/// The newest sample of each of `table_ids`, keyed by table.
///
/// Batch form of [`latest`] for the tables list and its health filter, which
/// evaluate a page or a whole visible inventory at once. Tables without a
/// sample are absent from the result, and the order is the table id order the
/// query plans on.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn latest_for_tables(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_ids: &[TableId],
) -> Result<Vec<(TableId, StoredTableSample)>, PersistenceError> {
    let ids: Vec<Uuid> = table_ids.iter().map(|table| *table.as_uuid()).collect();
    let rows: Vec<TableSampleRow> = sqlx::query_as(concat!(
        "SELECT DISTINCT ON (table_identity_id) table_identity_id, sampled_at, \
         catalog_connection_id, ",
        sample_columns!(),
        " FROM table_statistics WHERE organization_id = $1 AND table_identity_id = ANY($2) \
         ORDER BY table_identity_id, sampled_at DESC"
    ))
    .bind(organization_id.as_uuid())
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                TableId::from_uuid(row.table_identity_id),
                StoredTableSample::from(row.sample),
            )
        })
        .collect())
}

/// When each of `table_ids` was last confirmed fresh, omitting tables never
/// checked.
///
/// Batch form of the check half of [`last_confirmed_at`]; a caller that also
/// holds the newest sample folds the sample's instant in, as that function
/// does.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn checks_for_tables(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_ids: &[TableId],
) -> Result<Vec<(TableId, DateTime<Utc>)>, PersistenceError> {
    let ids: Vec<Uuid> = table_ids.iter().map(|table| *table.as_uuid()).collect();
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT table_identity_id, checked_at FROM table_statistics_checks \
         WHERE organization_id = $1 AND table_identity_id = ANY($2) \
         ORDER BY table_identity_id",
    )
    .bind(organization_id.as_uuid())
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(table, checked_at)| (TableId::from_uuid(table), checked_at))
        .collect())
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

/// Tables whose latest confirmation is at or before `cutoff`, oldest first.
///
#[derive(sqlx::FromRow)]
struct DueRow {
    organization_id: Uuid,
    table_id: Uuid,
    table_uuid: Uuid,
    last_confirmed_at: Option<DateTime<Utc>>,
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
                    COALESCE(checks.checked_at, latest.sampled_at) AS last_confirmed_at \
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
             LEFT JOIN table_statistics_checks checks \
               ON checks.organization_id = t.organization_id \
              AND checks.table_identity_id = t.id \
             WHERE COALESCE(checks.checked_at, latest.sampled_at) IS NULL \
                OR COALESCE(checks.checked_at, latest.sampled_at) <= $1 \
             ORDER BY COALESCE(checks.checked_at, latest.sampled_at) ASC NULLS FIRST, t.id \
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
            last_confirmed_at: row.last_confirmed_at,
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

/// One snapshot to append to the ledger.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTableSnapshot {
    /// The connection the table was read through.
    pub catalog_connection_id: CatalogConnectionId,
    /// The snapshot ID.
    pub snapshot_id: i64,
    /// The snapshot this commit replaced.
    pub parent_snapshot_id: Option<i64>,
    /// The sequence number the commit was assigned.
    pub sequence_number: i64,
    /// When the commit happened.
    pub committed_at: DateTime<Utc>,
    /// The operation, such as `append` or `replace`.
    pub operation: String,
    /// The committing engine's summary properties, as one JSON object.
    pub summary: Value,
}

/// One recorded snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredTableSnapshot {
    /// The snapshot ID.
    pub snapshot_id: i64,
    /// The snapshot this commit replaced.
    pub parent_snapshot_id: Option<i64>,
    /// The sequence number the commit was assigned.
    pub sequence_number: i64,
    /// When the commit happened.
    pub committed_at: DateTime<Utc>,
    /// The operation, such as `append` or `replace`.
    pub operation: String,
    /// The committing engine's summary properties, as one JSON object.
    pub summary: Value,
}

/// One hour of commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitHour {
    /// The start of the hour, in UTC.
    pub hour: DateTime<Utc>,
    /// Snapshots committed in that hour.
    pub commits: i64,
}

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    snapshot_id: i64,
    parent_snapshot_id: Option<i64>,
    sequence_number: i64,
    committed_at: DateTime<Utc>,
    operation: String,
    summary: Value,
}

impl From<SnapshotRow> for StoredTableSnapshot {
    fn from(row: SnapshotRow) -> Self {
        Self {
            snapshot_id: row.snapshot_id,
            parent_snapshot_id: row.parent_snapshot_id,
            sequence_number: row.sequence_number,
            committed_at: row.committed_at,
            operation: row.operation,
            summary: row.summary,
        }
    }
}

/// Appends `snapshots`, skipping any the ledger already holds, and stamps
/// them with `observed_at`.
///
/// Returns how many rows were inserted. A snapshot is never rewritten, so a
/// repeated pass inserts nothing.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when the table or connection is not
/// in the organization, and [`PersistenceError::Database`] when the query
/// fails.
pub async fn record_snapshots(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    snapshots: &[NewTableSnapshot],
    observed_at: DateTime<Utc>,
) -> Result<u64, PersistenceError> {
    const ENTITY: &str = "table snapshots";
    let Some(first) = snapshots.first() else {
        return Ok(0);
    };
    let connection = first.catalog_connection_id;
    let mut snapshot_ids = Vec::with_capacity(snapshots.len());
    let mut parent_ids = Vec::with_capacity(snapshots.len());
    let mut sequence_numbers = Vec::with_capacity(snapshots.len());
    let mut committed_at = Vec::with_capacity(snapshots.len());
    let mut operations = Vec::with_capacity(snapshots.len());
    let mut summaries = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        snapshot_ids.push(snapshot.snapshot_id);
        parent_ids.push(snapshot.parent_snapshot_id);
        sequence_numbers.push(snapshot.sequence_number);
        committed_at.push(snapshot.committed_at);
        operations.push(snapshot.operation.clone());
        summaries.push(snapshot.summary.to_string());
    }
    let result = sqlx::query(
        "INSERT INTO table_snapshots (organization_id, table_identity_id, \
         catalog_connection_id, snapshot_id, parent_snapshot_id, sequence_number, \
         committed_at, operation, summary, observed_at) \
         SELECT $1, $2, $3, u.snapshot_id, u.parent_snapshot_id, u.sequence_number, \
                u.committed_at, u.operation, u.summary::jsonb, $4 \
         FROM UNNEST($5::bigint[], $6::bigint[], $7::bigint[], $8::timestamptz[], \
                     $9::text[], $10::text[]) \
              AS u(snapshot_id, parent_snapshot_id, sequence_number, committed_at, \
                   operation, summary) \
         ON CONFLICT (organization_id, table_identity_id, snapshot_id) DO NOTHING",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(connection.as_uuid())
    .bind(observed_at)
    .bind(&snapshot_ids)
    .bind(&parent_ids)
    .bind(&sequence_numbers)
    .bind(&committed_at)
    .bind(&operations)
    .bind(&summaries)
    .execute(conn)
    .await
    .map_err(|error| PersistenceError::from_write(ENTITY, error))?;
    Ok(result.rows_affected())
}

/// Whether `snapshot_id` is recorded for `table_id`.
///
/// Membership is the question the collector asks: a rollback legitimately
/// leaves newer commits recorded, so the newest row says nothing about whether
/// the table's current snapshot is present. Two commits can also share an
/// instant, and a snapshot ID does not encode commit order.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn has_snapshot(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    snapshot_id: i64,
) -> Result<bool, PersistenceError> {
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM table_snapshots \
         WHERE organization_id = $1 AND table_identity_id = $2 AND snapshot_id = $3)",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(snapshot_id)
    .fetch_one(conn)
    .await?;
    Ok(present)
}

/// Snapshots of `table_id` newest first, starting strictly before `before`
/// and at or after `since`.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn snapshots(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    before: Option<(DateTime<Utc>, i64)>,
    since: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<StoredTableSnapshot>, PersistenceError> {
    let (before_at, before_id) = before.unzip();
    let rows: Vec<SnapshotRow> = sqlx::query_as(
        "SELECT snapshot_id, parent_snapshot_id, sequence_number, committed_at, \
                operation, summary \
         FROM table_snapshots WHERE organization_id = $1 AND table_identity_id = $2 \
         AND ($3::timestamptz IS NULL OR (committed_at, snapshot_id) < ($3, $4)) \
         AND ($5::timestamptz IS NULL OR committed_at >= $5) \
         ORDER BY committed_at DESC, snapshot_id DESC LIMIT $6",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(before_at)
    .bind(before_id.unwrap_or(i64::MAX))
    .bind(since)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(StoredTableSnapshot::from).collect())
}

#[derive(sqlx::FromRow)]
struct CommitHourRow {
    hour: chrono::NaiveDateTime,
    commits: i64,
}

/// Commits of `table_id` per hour between `since` and `until`, in ascending
/// hour order. Hours without a commit are absent.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn commit_hours(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CommitHour>, PersistenceError> {
    let rows: Vec<CommitHourRow> = sqlx::query_as(
        "SELECT date_trunc('hour', committed_at AT TIME ZONE 'UTC') AS hour, \
                count(*) AS commits \
         FROM table_snapshots WHERE organization_id = $1 AND table_identity_id = $2 \
         AND committed_at >= $3 AND committed_at < $4 \
         GROUP BY hour ORDER BY hour",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(since)
    .bind(until)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| CommitHour {
            hour: row.hour.and_utc(),
            commits: row.commits,
        })
        .collect())
}

/// Deletes at most `batch` snapshots observed before `older_than`, oldest
/// first.
///
/// Returns how many rows were deleted.
///
/// # Errors
///
/// [`PersistenceError::InvalidValue`] when the batch is not positive, and
/// [`PersistenceError::Database`] when the query fails.
pub async fn prune_snapshots(
    conn: &mut PgConnection,
    older_than: DateTime<Utc>,
    batch: i64,
) -> Result<u64, PersistenceError> {
    if batch <= 0 {
        return Err(PersistenceError::InvalidValue {
            entity: "table snapshots",
            constraint: "prune_batch_positive".to_owned(),
        });
    }
    let result = sqlx::query(
        "DELETE FROM table_snapshots WHERE ctid IN ( \
             SELECT ctid FROM table_snapshots WHERE observed_at < $1 \
             ORDER BY observed_at LIMIT $2)",
    )
    .bind(older_than)
    .bind(batch)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

/// Records the newest observed health state and reports whether the table
/// entered Critical.
///
/// The previous state is read under the row lock, so two collectors cannot both
/// observe the same transition. A table with no assessment stores `NULL`;
/// recovery to a milder state lets a later Critical entry alert again.
///
/// # Errors
///
/// Database errors.
pub async fn record_health_state(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_identity_id: TableId,
    state: Option<&str>,
    at: DateTime<Utc>,
) -> Result<bool, PersistenceError> {
    let previous: Option<Option<String>> = sqlx::query_scalar(
        "SELECT state FROM table_health_states
         WHERE organization_id = $1 AND table_identity_id = $2
         FOR UPDATE",
    )
    .bind(organization_id.as_uuid())
    .bind(table_identity_id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let previous = previous.flatten();
    sqlx::query(
        "INSERT INTO table_health_states
             (organization_id, table_identity_id, state, observed_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (organization_id, table_identity_id) DO UPDATE
         SET state = EXCLUDED.state, observed_at = EXCLUDED.observed_at",
    )
    .bind(organization_id.as_uuid())
    .bind(table_identity_id.as_uuid())
    .bind(state)
    .bind(at)
    .execute(&mut *conn)
    .await?;
    Ok(previous.as_deref() != Some("critical") && state == Some("critical"))
}
