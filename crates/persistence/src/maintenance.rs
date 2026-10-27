//! Maintenance impact aggregates over the execution ledger (BS-027.T2).
//!
//! Every figure comes from immutable rows: a job's terminal state, the plan it
//! was built from, the receipts its workers returned, and the output objects
//! verified at acceptance. Failed and superseded attempts never contribute, so
//! a retried task counts once, through the attempt that was accepted.
//!
//! History and totals share one definition of impact. Input files and bytes
//! are the sources of accepted tasks, which is the work actually rewritten.
//! Accepted output is partitioned by the job's outcome so a byte is counted
//! once: `output_*` is published work (`Succeeded`, `NoOp`), `discarded_*` is
//! work a conflict threw away (`ReplanRequired`), and `abandoned_*` is work of
//! jobs that ended `Cancelled` or `Failed`. Delete files are counted once per
//! job, because a retained delete file can apply in several jobs.

use bergsmith_common::ids::{JobId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::job::{JobOperation, JobState};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::PersistenceError;
use crate::jobs::parse_job_state;

/// One terminal job's accepted impact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceEntry {
    /// The job.
    pub job_id: JobId,
    /// How the job ended.
    pub state: JobState,
    /// What the job did.
    pub operation: JobOperation,
    /// When the job was created.
    pub created_at: DateTime<Utc>,
    /// When it reached its terminal state.
    pub updated_at: DateTime<Utc>,
    /// End-to-end time from creation to the terminal state.
    pub elapsed_millis: i64,
    /// Source files of accepted tasks.
    pub input_files: i64,
    /// Bytes of those source files.
    pub input_bytes: i64,
    /// Output objects accepted with the tasks' receipts.
    pub output_files: i64,
    /// Bytes of those output objects.
    pub output_bytes: i64,
    /// Distinct delete files the accepted tasks applied.
    pub delete_files_applied: i64,
}

/// Impact over a set of terminal jobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceTotals {
    /// Jobs in the range (or page).
    pub jobs: i64,
    /// Jobs that ended `Succeeded`.
    pub succeeded: i64,
    /// Jobs that ended `NoOp`.
    pub no_op: i64,
    /// Jobs that ended `Cancelled`.
    pub cancelled: i64,
    /// Jobs that ended `ReplanRequired`, whose outputs were discarded.
    pub replanned: i64,
    /// Jobs that ended `Failed`.
    pub failed: i64,
    /// Source files of accepted tasks.
    pub input_files: i64,
    /// Bytes of those source files.
    pub input_bytes: i64,
    /// Output objects of published jobs (`Succeeded`, `NoOp`).
    pub output_files: i64,
    /// Bytes of those output objects.
    pub output_bytes: i64,
    /// Distinct delete files applied.
    pub delete_files_applied: i64,
    /// Output files of jobs that ended `ReplanRequired`.
    pub discarded_files: i64,
    /// Bytes of those discarded output files.
    pub discarded_bytes: i64,
    /// Output files of jobs that ended `Cancelled` or `Failed`, which were
    /// never published either.
    pub abandoned_files: i64,
    /// Bytes of those abandoned output files.
    pub abandoned_bytes: i64,
}

/// The accepted-impact CTEs shared by history and totals.
///
/// Each query begins with a `jobs_in` CTE (the terminal jobs in range) and
/// expands this to the accepted tasks, their plan sources, their deletes, and
/// their output objects. The queries are concatenations of literals, so the
/// SQL text is fixed at compile time and only the filters are bound.
macro_rules! impact {
    () => {
        "accepted AS ( \
             SELECT j.id AS job_id, rc.task_id, rc.attempt_id \
             FROM jobs_in j \
             JOIN tasks t ON t.job_id = j.id \
             JOIN receipts rc ON rc.task_id = t.id \
         ), plan_tasks AS MATERIALIZED ( \
             SELECT p.job_id, task \
             FROM jobs_in j \
             JOIN plans p ON p.job_id = j.id \
             CROSS JOIN LATERAL jsonb_array_elements(COALESCE(p.content->'tasks', '[]'::jsonb)) task \
         ), rewritten AS ( \
             SELECT a.job_id, pt.task \
             FROM accepted a \
             JOIN plan_tasks pt \
               ON pt.job_id = a.job_id AND pt.task->>'task_id' = a.task_id::text \
         ), sources AS ( \
             SELECT job_id, jsonb_array_elements(COALESCE(task->'sources', '[]'::jsonb)) source \
             FROM rewritten \
         ), deletes AS ( \
             SELECT job_id, jsonb_array_elements(COALESCE(task->'deletes', '[]'::jsonb)) entry \
             FROM rewritten \
         ), outputs AS ( \
             SELECT a.job_id, o.size_bytes \
             FROM attempt_output_objects o \
             JOIN accepted a ON a.attempt_id = o.attempt_id \
         )"
    };
}

/// The aggregate projection over `jobs_in`, `sources`, `deletes`, and
/// `outputs`, so totals cannot drift from the per-job values.
macro_rules! totals {
    () => {
        "totals AS ( \
             SELECT (SELECT count(*) FROM jobs_in)::bigint AS jobs, \
                    (SELECT count(*) FROM jobs_in WHERE state = 'Succeeded')::bigint AS succeeded, \
                    (SELECT count(*) FROM jobs_in WHERE state = 'NoOp')::bigint AS no_op, \
                    (SELECT count(*) FROM jobs_in WHERE state = 'Cancelled')::bigint AS cancelled, \
                    (SELECT count(*) FROM jobs_in WHERE state = 'ReplanRequired')::bigint AS replanned, \
                    (SELECT count(*) FROM jobs_in WHERE state = 'Failed')::bigint AS failed, \
                    (SELECT count(*) FROM sources)::bigint AS input_files, \
                    (SELECT COALESCE(sum((source->>'file_size_bytes')::numeric), 0)::bigint \
                     FROM sources) AS input_bytes, \
                    (SELECT count(*) FROM outputs o JOIN jobs_in j \
                        ON j.id = o.job_id AND j.state IN ('Succeeded', 'NoOp'))::bigint \
                        AS output_files, \
                    (SELECT COALESCE(sum(o.size_bytes), 0)::bigint FROM outputs o JOIN jobs_in j \
                        ON j.id = o.job_id AND j.state IN ('Succeeded', 'NoOp')) AS output_bytes, \
                    (SELECT count(*) FROM ( \
                        SELECT DISTINCT job_id, entry->>'path' FROM deletes \
                     ) applications)::bigint AS delete_files_applied, \
                    (SELECT count(*) FROM outputs o JOIN jobs_in j \
                        ON j.id = o.job_id AND j.state = 'ReplanRequired')::bigint \
                        AS discarded_files, \
                    (SELECT COALESCE(sum(o.size_bytes), 0)::bigint FROM outputs o JOIN jobs_in j \
                        ON j.id = o.job_id AND j.state = 'ReplanRequired') AS discarded_bytes, \
                    (SELECT count(*) FROM outputs o JOIN jobs_in j \
                        ON j.id = o.job_id AND j.state IN ('Cancelled', 'Failed'))::bigint \
                        AS abandoned_files, \
                    (SELECT COALESCE(sum(o.size_bytes), 0)::bigint FROM outputs o JOIN jobs_in j \
                        ON j.id = o.job_id AND j.state IN ('Cancelled', 'Failed')) \
                        AS abandoned_bytes \
         )"
    };
}

const HISTORY_SQL: &str = concat!(
    "WITH jobs_in AS ( \
         SELECT j.id, j.state, j.operation, j.created_at, j.updated_at \
         FROM jobs j \
         WHERE j.organization_id = $1 AND j.table_identity_id = $2 \
           AND j.state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed') \
           AND j.created_at >= $3 \
           AND ($4::uuid IS NULL OR j.id < $4::uuid) \
         ORDER BY j.id DESC \
         LIMIT $5 \
     ), ",
    impact!(),
    " SELECT j.id, j.state, j.operation, j.created_at, j.updated_at, \
            FLOOR(GREATEST(0, EXTRACT(EPOCH FROM (j.updated_at - j.created_at)) * 1000))::bigint \
                AS elapsed_millis, \
            (SELECT count(*) FROM sources s WHERE s.job_id = j.id) AS input_files, \
            (SELECT COALESCE(sum((source->>'file_size_bytes')::numeric), 0)::bigint \
             FROM sources s WHERE s.job_id = j.id) AS input_bytes, \
            (SELECT count(*) FROM outputs o WHERE o.job_id = j.id) AS output_files, \
            (SELECT COALESCE(sum(o.size_bytes), 0)::bigint \
             FROM outputs o WHERE o.job_id = j.id) AS output_bytes, \
            (SELECT count(DISTINCT entry->>'path') FROM deletes d WHERE d.job_id = j.id) \
                AS delete_files_applied \
     FROM jobs_in j \
     ORDER BY j.id DESC"
);

const TABLE_TOTALS_SQL: &str = concat!(
    "WITH jobs_in AS ( \
         SELECT j.id, j.state \
         FROM jobs j \
         WHERE j.organization_id = $1 AND j.table_identity_id = $2 \
           AND j.state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed') \
           AND j.created_at >= $3 \
     ), ",
    impact!(),
    ", ",
    totals!(),
    " SELECT * FROM totals"
);

const ORGANIZATION_TOTALS_SQL: &str = concat!(
    "WITH jobs_in AS ( \
         SELECT j.id, j.state \
         FROM jobs j \
         JOIN table_identities t \
           ON t.id = j.table_identity_id AND t.organization_id = j.organization_id \
         WHERE j.organization_id = $1 \
           AND j.state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed') \
           AND j.created_at >= $4 \
           AND EXISTS ( \
               SELECT 1 FROM principal_role_grants g \
               WHERE g.organization_id = $1 AND g.principal_id = $2 \
                 AND g.revoked_at IS NULL AND g.role = ANY($3::text[]) \
                 AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL) \
                   OR g.table_identity_id = t.id \
                   OR EXISTS (SELECT 1 FROM table_aliases a \
                              WHERE a.organization_id = $1 AND a.table_identity_id = t.id \
                                AND a.catalog_connection_id = g.catalog_connection_id))) \
     ), ",
    impact!(),
    ", ",
    totals!(),
    " SELECT * FROM totals"
);

/// One page of `table`'s terminal jobs, newest first.
///
/// `after` is the cursor: only jobs with a smaller ID (older, because IDs are
/// `UUIDv7`) are returned. The caller requests one row more than the page to
/// learn whether another page exists.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails or a stored state is unknown.
pub async fn history(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table: TableId,
    since: DateTime<Utc>,
    after: Option<JobId>,
    limit: i64,
) -> Result<Vec<MaintenanceEntry>, PersistenceError> {
    let rows: Vec<EntryRow> = sqlx::query_as(HISTORY_SQL)
        .bind(organization_id.as_uuid())
        .bind(table.as_uuid())
        .bind(since)
        .bind(after.map(|id| *id.as_uuid()))
        .bind(limit)
        .fetch_all(conn)
        .await?;
    rows.into_iter().map(EntryRow::into_entry).collect()
}

/// Impact totals of `table`'s terminal jobs in the range.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn table_totals(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table: TableId,
    since: DateTime<Utc>,
) -> Result<MaintenanceTotals, PersistenceError> {
    let row: TotalsRow = sqlx::query_as(TABLE_TOTALS_SQL)
        .bind(organization_id.as_uuid())
        .bind(table.as_uuid())
        .bind(since)
        .fetch_one(conn)
        .await?;
    Ok(row.into())
}

/// Impact totals over every table the caller may view in the organization.
///
/// The grant predicate matches the one every job read uses: an
/// organization-wide grant covers every table, a connection grant covers the
/// tables registered on it, and a table grant covers that table.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn organization_totals(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[String],
    since: DateTime<Utc>,
) -> Result<MaintenanceTotals, PersistenceError> {
    let row: TotalsRow = sqlx::query_as(ORGANIZATION_TOTALS_SQL)
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(roles)
        .bind(since)
        .fetch_one(conn)
        .await?;
    Ok(row.into())
}

#[derive(sqlx::FromRow)]
struct EntryRow {
    id: uuid::Uuid,
    state: String,
    operation: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    elapsed_millis: i64,
    input_files: i64,
    input_bytes: i64,
    output_files: i64,
    output_bytes: i64,
    delete_files_applied: i64,
}

impl EntryRow {
    fn into_entry(self) -> Result<MaintenanceEntry, PersistenceError> {
        Ok(MaintenanceEntry {
            job_id: JobId::from_uuid(self.id),
            state: parse_job_state(&self.state)?,
            operation: JobOperation::from_name(&self.operation).ok_or_else(|| {
                PersistenceError::InvalidValue {
                    entity: "maintenance entry",
                    constraint: format!("unknown job operation {:?}", self.operation),
                }
            })?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            elapsed_millis: self.elapsed_millis,
            input_files: self.input_files,
            input_bytes: self.input_bytes,
            output_files: self.output_files,
            output_bytes: self.output_bytes,
            delete_files_applied: self.delete_files_applied,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TotalsRow {
    jobs: i64,
    succeeded: i64,
    no_op: i64,
    cancelled: i64,
    replanned: i64,
    failed: i64,
    input_files: i64,
    input_bytes: i64,
    output_files: i64,
    output_bytes: i64,
    delete_files_applied: i64,
    discarded_files: i64,
    discarded_bytes: i64,
    abandoned_files: i64,
    abandoned_bytes: i64,
}

impl From<TotalsRow> for MaintenanceTotals {
    fn from(row: TotalsRow) -> Self {
        Self {
            jobs: row.jobs,
            succeeded: row.succeeded,
            no_op: row.no_op,
            cancelled: row.cancelled,
            replanned: row.replanned,
            failed: row.failed,
            input_files: row.input_files,
            input_bytes: row.input_bytes,
            output_files: row.output_files,
            output_bytes: row.output_bytes,
            delete_files_applied: row.delete_files_applied,
            discarded_files: row.discarded_files,
            discarded_bytes: row.discarded_bytes,
            abandoned_files: row.abandoned_files,
            abandoned_bytes: row.abandoned_bytes,
        }
    }
}
