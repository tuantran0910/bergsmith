//! Publication: sending a job's commit across the cancellation boundary.
//!
//! The first send of a job's commit is authorized by a short database
//! transaction that commits before any network call (see
//! [`authorize_send`]). Once it commits, cancellation can no longer prevent
//! publication. [`crate::durable_publication`] owns intent construction and
//! outcome recovery; this lower-level sender binds every request to its intent.

use bergsmith_catalog::{
    CatalogConnection, CatalogError, CommitOutcome, CommitRequest, IdempotencyKey,
};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::cancellation::{SendAuthorization, authorize_send};
use bergsmith_persistence::jobs::Job;
use sqlx::PgConnection;

/// The first send of a job's commit.
#[derive(Debug)]
pub struct SentCommit {
    /// The job with `publication_started_at` set.
    pub job: Job,
    /// What the catalog did.
    pub outcome: CommitOutcome,
}

/// Why the first send did not reach the catalog.
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    /// Send authorization was refused, for example because cancellation was
    /// requested first. Nothing was authorized or sent.
    #[error("the commit send was not authorized")]
    NotAuthorized(#[source] PersistenceError),
    /// The request could not be prepared, for example because no OAuth2
    /// token could be fetched. Nothing was authorized or sent, so the same
    /// intent may still be published later.
    #[error("the commit request could not be prepared")]
    NotPrepared(#[source] CatalogError),
    /// The send was authorized, but the request was never sent: no
    /// connection to the catalog could be established.
    #[error("the authorized commit for job {} was not sent", .job.id)]
    NotSent {
        /// The job with `publication_started_at` set.
        job: Box<Job>,
        /// Why the request was not sent.
        #[source]
        source: CatalogError,
    },
}

/// Authorizes the first send of a job's commit, then sends it once.
///
/// Everything that can fail before the request leaves, other than connecting
/// (intent binding, URL construction, and fetching an OAuth2 token), happens
/// before authorization, so those failures leave the job unauthorized and
/// retryable. The authorization transaction commits before the request
/// leaves, so no database transaction is held across the catalog call.
///
/// # Errors
///
/// [`PublicationError::NotAuthorized`] with the error of [`authorize_send`],
/// [`PublicationError::NotPrepared`] when the request could not be
/// authenticated, and [`PublicationError::NotSent`] when the authorized
/// request could not be sent.
pub async fn send_first_commit(
    conn: &mut PgConnection,
    authorization: SendAuthorization,
    catalog: &CatalogConnection,
    request: &CommitRequest,
    idempotency_key: Option<IdempotencyKey>,
) -> Result<SentCommit, PublicationError> {
    let intent = bergsmith_persistence::publications::intent(conn, authorization.job_id)
        .await
        .map_err(PublicationError::NotAuthorized)?;
    if intent.request_body != request.body()
        || intent.request_hash != request.sha256().to_hex()
        || idempotency_key.is_some()
        || intent.content.get("namespace") != Some(&serde_json::json!(request.table().namespace))
        || intent
            .content
            .get("table_name")
            .and_then(serde_json::Value::as_str)
            != Some(request.table().name.as_str())
    {
        return Err(PublicationError::NotAuthorized(
            PersistenceError::InvalidValue {
                entity: "publication request",
                constraint: "request must match the immutable intent and replay policy".into(),
            },
        ));
    }
    let prepared = catalog
        .authenticate_commit(request, idempotency_key)
        .await
        .map_err(PublicationError::NotPrepared)?;
    let job = authorize_send(conn, authorization)
        .await
        .map_err(PublicationError::NotAuthorized)?;
    match prepared.send().await {
        Ok(outcome) => Ok(SentCommit { job, outcome }),
        Err(source) => Err(PublicationError::NotSent {
            job: Box::new(job),
            source,
        }),
    }
}
