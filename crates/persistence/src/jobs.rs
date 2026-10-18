//! Jobs and their append-only event history.

use bergsmith_common::ids::{JobId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::job::{JobState, PublicationContext};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;

/// Columns selected for [`Job`], in [`JobRow`] order. A macro so queries stay
/// `&'static str` literals.
macro_rules! job_columns {
    () => {
        "id, organization_id, table_identity_id, state, state_revision, state_reason, \
         retry_of_job_id, created_by, created_at, updated_at, cancellation_requested_at, \
         cancellation_requested_by, cancellation_reason, publication_started_at"
    };
}
pub(crate) use job_columns;

/// A maintenance job as persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Identifier.
    pub id: JobId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The canonical table the job maintains.
    pub table_id: TableId,
    /// Current lifecycle state.
    pub state: JobState,
    /// Incremented by every state transition; writers supply the revision
    /// they read so concurrent transitions cannot both succeed.
    pub state_revision: i64,
    /// Typed reason for the current state, if it has one.
    pub state_reason: Option<String>,
    /// The terminal job this one retries with a fresh plan, if any.
    pub retry_of: Option<JobId>,
    /// Who submitted the job, if a principal did.
    pub created_by: Option<PrincipalId>,
    /// When the job was created (database time).
    pub created_at: DateTime<Utc>,
    /// When the job row last changed (database time).
    pub updated_at: DateTime<Utc>,
    /// The first cancellation request, if any. Requests are monotonic: a
    /// recorded request never changes.
    pub cancellation: Option<CancellationRequest>,
    /// When the first outbound commit request was authorized. Once set,
    /// cancellation can no longer prevent publication.
    pub publication_started_at: Option<DateTime<Utc>>,
}

/// A recorded cancellation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancellationRequest {
    /// When the first request was recorded (database time).
    pub requested_at: DateTime<Utc>,
    /// Who asked, if a principal did.
    pub requested_by: Option<PrincipalId>,
    /// Operator-supplied reason, if any.
    pub reason: Option<String>,
}

/// Fields for a new job. Jobs always start in [`JobState::Planning`].
#[derive(Clone, Debug)]
pub struct NewJob {
    /// Identifier to assign.
    pub id: JobId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The canonical table to maintain; must belong to the organization.
    pub table_id: TableId,
    /// The terminal job this one retries, if any.
    pub retry_of: Option<JobId>,
    /// Who submitted the job, if a principal did.
    pub created_by: Option<PrincipalId>,
}

/// The kind of a [`JobEvent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEventKind {
    /// The job was created in `Planning`.
    Created,
    /// The job changed state.
    Transitioned,
    /// Cancellation was requested.
    CancellationRequested,
    /// The first outbound commit request was authorized.
    PublicationAuthorized,
}

impl JobEventKind {
    /// Returns the persisted name of the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Transitioned => "Transitioned",
            Self::CancellationRequested => "CancellationRequested",
            Self::PublicationAuthorized => "PublicationAuthorized",
        }
    }

    fn parse(value: &str) -> Result<Self, PersistenceError> {
        match value {
            "Created" => Ok(Self::Created),
            "Transitioned" => Ok(Self::Transitioned),
            "CancellationRequested" => Ok(Self::CancellationRequested),
            "PublicationAuthorized" => Ok(Self::PublicationAuthorized),
            other => Err(PersistenceError::CorruptValue {
                entity: "job event kind",
                value: other.to_owned(),
            }),
        }
    }
}

/// One entry in a job's append-only history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEvent {
    /// What happened.
    pub kind: JobEventKind,
    /// State before a transition; `None` for other kinds.
    pub from_state: Option<JobState>,
    /// State after the event.
    pub to_state: JobState,
    /// Job revision after the event.
    pub state_revision: i64,
    /// Typed reason, if any.
    pub reason: Option<String>,
    /// Principal that caused the event, if any.
    pub actor: Option<PrincipalId>,
    /// When the event was recorded (database time).
    pub occurred_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct JobRow {
    id: Uuid,
    organization_id: Uuid,
    table_identity_id: Uuid,
    state: String,
    state_revision: i64,
    state_reason: Option<String>,
    retry_of_job_id: Option<Uuid>,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    cancellation_requested_at: Option<DateTime<Utc>>,
    cancellation_requested_by: Option<Uuid>,
    cancellation_reason: Option<String>,
    publication_started_at: Option<DateTime<Utc>>,
}

impl TryFrom<JobRow> for Job {
    type Error = PersistenceError;

    fn try_from(row: JobRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: JobId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            table_id: TableId::from_uuid(row.table_identity_id),
            state: parse_job_state(&row.state)?,
            state_revision: row.state_revision,
            state_reason: row.state_reason,
            retry_of: row.retry_of_job_id.map(JobId::from_uuid),
            created_by: row.created_by.map(PrincipalId::from_uuid),
            created_at: row.created_at,
            updated_at: row.updated_at,
            cancellation: row
                .cancellation_requested_at
                .map(|requested_at| CancellationRequest {
                    requested_at,
                    requested_by: row.cancellation_requested_by.map(PrincipalId::from_uuid),
                    reason: row.cancellation_reason,
                }),
            publication_started_at: row.publication_started_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct JobEventRow {
    kind: String,
    from_state: Option<String>,
    to_state: String,
    state_revision: i64,
    reason: Option<String>,
    actor_id: Option<Uuid>,
    occurred_at: DateTime<Utc>,
}

impl TryFrom<JobEventRow> for JobEvent {
    type Error = PersistenceError;

    fn try_from(row: JobEventRow) -> Result<Self, Self::Error> {
        Ok(Self {
            kind: JobEventKind::parse(&row.kind)?,
            from_state: row.from_state.as_deref().map(parse_job_state).transpose()?,
            to_state: parse_job_state(&row.to_state)?,
            state_revision: row.state_revision,
            reason: row.reason,
            actor: row.actor_id.map(PrincipalId::from_uuid),
            occurred_at: row.occurred_at,
        })
    }
}

pub(crate) fn parse_job_state(value: &str) -> Result<JobState, PersistenceError> {
    JobState::from_name(value).ok_or_else(|| PersistenceError::CorruptValue {
        entity: "job state",
        value: value.to_owned(),
    })
}

/// An event to append, in the same transaction as the change it records.
pub(crate) struct EventRecord<'a> {
    pub(crate) job_id: JobId,
    pub(crate) kind: JobEventKind,
    pub(crate) from_state: Option<JobState>,
    pub(crate) to_state: JobState,
    pub(crate) state_revision: i64,
    pub(crate) reason: Option<&'a str>,
    pub(crate) actor: Option<PrincipalId>,
}

pub(crate) async fn append_event(
    conn: &mut PgConnection,
    event: &EventRecord<'_>,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "INSERT INTO job_events
             (job_id, kind, from_state, to_state, state_revision, reason, actor_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(event.job_id.as_uuid())
    .bind(event.kind.as_str())
    .bind(event.from_state.map(JobState::as_str))
    .bind(event.to_state.as_str())
    .bind(event.state_revision)
    .bind(event.reason)
    .bind(event.actor.as_ref().map(PrincipalId::as_uuid))
    .execute(conn)
    .await
    .map_err(|error| PersistenceError::from_write("job event", error))?;
    Ok(())
}

/// Locks a job row for the rest of the transaction and returns it.
pub(crate) async fn lock_job(conn: &mut PgConnection, id: JobId) -> Result<Job, PersistenceError> {
    sqlx::query_as::<_, JobRow>(concat!(
        "SELECT ",
        job_columns!(),
        " FROM jobs WHERE id = $1 FOR UPDATE"
    ))
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| job_not_found(id))?
    .try_into()
}

pub(crate) fn job_not_found(id: JobId) -> PersistenceError {
    PersistenceError::NotFound {
        entity: "job",
        id: id.to_string(),
    }
}

/// Creates a job in [`JobState::Planning`] and records its `Created` event.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the ID is taken and
/// [`PersistenceError::MissingReference`] when the table, retried job, or
/// creator does not exist in the organization.
pub async fn create_job(conn: &mut PgConnection, new: &NewJob) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job: Job = sqlx::query_as::<_, JobRow>(concat!(
        "INSERT INTO jobs (id, organization_id, table_identity_id, state, retry_of_job_id, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING ",
        job_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.table_id.as_uuid())
    .bind(JobState::Planning.as_str())
    .bind(new.retry_of.as_ref().map(JobId::as_uuid))
    .bind(new.created_by.as_ref().map(PrincipalId::as_uuid))
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("job", error))?
    .try_into()?;
    append_event(
        &mut tx,
        &EventRecord {
            job_id: job.id,
            kind: JobEventKind::Created,
            from_state: None,
            to_state: job.state,
            state_revision: job.state_revision,
            reason: None,
            actor: job.created_by,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(job)
}

/// Reads a job within an organization.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such job.
pub async fn job(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: JobId,
) -> Result<Job, PersistenceError> {
    sqlx::query_as::<_, JobRow>(concat!(
        "SELECT ",
        job_columns!(),
        " FROM jobs WHERE organization_id = $1 AND id = $2"
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| job_not_found(id))?
    .try_into()
}

/// Returns a job's events, oldest first.
///
/// # Errors
///
/// [`PersistenceError::CorruptValue`] when a stored event does not decode.
pub async fn job_events(
    conn: &mut PgConnection,
    id: JobId,
) -> Result<Vec<JobEvent>, PersistenceError> {
    sqlx::query_as::<_, JobEventRow>(
        "SELECT kind, from_state, to_state, state_revision, reason, actor_id, occurred_at
         FROM job_events WHERE job_id = $1 ORDER BY id",
    )
    .bind(id.as_uuid())
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(JobEvent::try_from)
    .collect()
}

/// A requested state change for a job.
#[derive(Clone, Debug)]
pub struct Transition {
    /// The job to change.
    pub job_id: JobId,
    /// The revision the caller read. The change applies only if it is still current.
    pub expected_revision: i64,
    /// The requested state.
    pub to: JobState,
    /// Typed reason for the new state, if any.
    pub reason: Option<String>,
    /// Principal causing the change, if any.
    pub actor: Option<PrincipalId>,
    /// Conclusive evidence shows no authorized commit request published or can
    /// still publish. Required for a non-success ending after send
    /// authorization.
    pub non_publication_proven: bool,
}

/// Applies a revision-checked state transition and records its event in the
/// same transaction.
///
/// The job row is locked, its revision compared with `expected_revision`, and
/// the change validated with [`JobState::validate_transition`] before it is
/// written. The revision then increases by one.
///
/// # Errors
///
/// - [`PersistenceError::RevisionConflict`] when another writer changed the
///   job since the caller read it.
/// - [`PersistenceError::JobTransition`] when the lifecycle rules forbid the
///   change, including any change to a terminal job.
/// - [`PersistenceError::NotFound`] when the job does not exist.
pub async fn transition_job(
    conn: &mut PgConnection,
    transition: &Transition,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let current = lock_job(&mut tx, transition.job_id).await?;
    if current.state_revision != transition.expected_revision {
        return Err(PersistenceError::RevisionConflict {
            job: current.id,
            expected: transition.expected_revision,
            actual: current.state_revision,
        });
    }
    current.state.validate_transition(
        transition.to,
        PublicationContext {
            send_authorized: current.publication_started_at.is_some(),
            non_publication_proven: transition.non_publication_proven,
        },
    )?;

    let job: Job = sqlx::query_as::<_, JobRow>(concat!(
        "UPDATE jobs SET state = $2, state_revision = state_revision + 1, state_reason = $3,
             updated_at = now()
         WHERE id = $1
         RETURNING ",
        job_columns!()
    ))
    .bind(current.id.as_uuid())
    .bind(transition.to.as_str())
    .bind(&transition.reason)
    .fetch_one(&mut *tx)
    .await?
    .try_into()?;
    append_event(
        &mut tx,
        &EventRecord {
            job_id: job.id,
            kind: JobEventKind::Transitioned,
            from_state: Some(current.state),
            to_state: job.state,
            state_revision: job.state_revision,
            reason: transition.reason.as_deref(),
            actor: transition.actor,
        },
    )
    .await?;
    if job.state.is_terminal() || job.state == JobState::Cancelling {
        sqlx::query("UPDATE attempts SET state='Cancelled',finished_at=now() WHERE task_id IN (SELECT id FROM tasks WHERE job_id=$1) AND state IN ('Granted','Running')")
            .bind(job.id.as_uuid()).execute(&mut *tx).await?;
        sqlx::query("UPDATE tasks SET state='Cancelled',retry_not_before=NULL,updated_at=now() WHERE job_id=$1 AND state IN ('Pending','Leased','RetryWaiting')")
            .bind(job.id.as_uuid()).execute(&mut *tx).await?;
    }
    if job.state.is_terminal() {
        sqlx::query(
            "UPDATE execution_admissions SET released=true WHERE job_id=$1 AND NOT released",
        )
        .bind(job.id.as_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM publication_barriers WHERE job_id = $1")
            .bind(job.id.as_uuid())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kinds_round_trip_through_their_persisted_names() {
        for kind in [
            JobEventKind::Created,
            JobEventKind::Transitioned,
            JobEventKind::CancellationRequested,
            JobEventKind::PublicationAuthorized,
        ] {
            assert_eq!(JobEventKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(JobEventKind::parse("Deleted").is_err());
    }

    #[test]
    fn unknown_job_states_are_reported_as_corrupt() {
        let error = parse_job_state("Paused").unwrap_err();
        assert!(matches!(
            error,
            PersistenceError::CorruptValue {
                entity: "job state",
                ..
            }
        ));
    }
}
