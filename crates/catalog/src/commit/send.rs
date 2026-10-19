//! Sending a commit and classifying the response (BS-006.T3).
//!
//! A commit is sent once. Nothing here retries, follows redirects, or
//! refreshes the request. The outcome says whether the catalog definitely
//! published it, definitely rejected it, or may have done either.

use std::fmt;

use bergsmith_common::disposition::FailureDisposition;
use iceberg::spec::TableMetadata;
use iceberg_catalog_rest::CommitTableResponse;
use reqwest::Method;
use reqwest::header::CONTENT_TYPE;
use uuid::Uuid;

use crate::commit::request::CommitRequest;
use crate::connection::CatalogConnection;
use crate::error::{CatalogError, CatalogErrorModel, ErrorEnvelope};

/// The header carrying a standard REST idempotency key.
const IDEMPOTENCY_KEY: &str = "Idempotency-Key";

const OPERATION: &str = "POST commit table";

/// A standard REST idempotency key, a `UUIDv7`. A replay of the same request
/// must reuse the key it was first sent with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(Uuid);

impl IdempotencyKey {
    /// A new key.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// Restores a persisted key.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The key's UUID, to persist.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What the catalog did with a commit that was sent.
#[derive(Debug)]
pub enum CommitOutcome {
    /// The catalog published the commit.
    Committed(Box<Committed>),
    /// The catalog definitely did not apply this request. If an earlier
    /// send of the same intent is unresolved, this proves nothing about it.
    Rejected(CommitRejection),
    /// The commit may or may not have been published.
    Unknown(UnknownOutcome),
}

/// A successful commit response.
#[derive(Debug)]
pub struct Committed {
    /// The new metadata file, when the response body decoded.
    pub metadata_location: Option<String>,
    /// The new metadata, when the response body decoded. A success status
    /// is conclusive even without it.
    pub metadata: Option<TableMetadata>,
}

/// A definitive rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRejection {
    /// Why.
    pub kind: RejectionKind,
    /// The HTTP status.
    pub status: u16,
    /// The catalog's error body, if it sent one.
    pub model: Option<CatalogErrorModel>,
}

/// The statuses the REST specification defines as "not applied".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionKind {
    /// 409: a requirement failed; the baseline changed.
    RequirementFailed,
    /// 400: the catalog considers the request malformed.
    BadRequest,
    /// 401 or 419: the catalog did not accept the credentials.
    Unauthorized,
    /// 403: the principal may not commit to the table.
    Forbidden,
    /// 404: the table or namespace does not exist.
    NotFound,
    /// 406: the catalog does not support an update in the request.
    Unsupported,
}

/// Why a commit's outcome is unknown.
#[derive(Debug)]
pub enum UnknownOutcome {
    /// Any status not defined as success or rejection, such as 5xx or a
    /// proxy's 429.
    Status {
        /// The HTTP status.
        status: u16,
        /// The error body, if it decoded.
        model: Option<CatalogErrorModel>,
    },
    /// No response within the request timeout.
    Timeout,
    /// The connection failed after the request may have been sent.
    Transport(reqwest::Error),
}

impl CommitOutcome {
    /// The disposition of a failed commit; `None` once committed.
    ///
    /// A requirement failure or a missing table means the baseline no
    /// longer holds; other rejections cannot succeed on retry; an unknown
    /// outcome must be resolved from evidence.
    #[must_use]
    pub fn disposition(&self) -> Option<FailureDisposition> {
        match self {
            Self::Committed(_) => None,
            Self::Rejected(rejection) => Some(match rejection.kind {
                RejectionKind::RequirementFailed | RejectionKind::NotFound => {
                    FailureDisposition::ReplanRequired
                }
                RejectionKind::BadRequest
                | RejectionKind::Unauthorized
                | RejectionKind::Forbidden
                | RejectionKind::Unsupported => FailureDisposition::PermanentFailure,
            }),
            Self::Unknown(_) => Some(FailureDisposition::ResolvePublication),
        }
    }

    /// Classifies a response.
    pub(crate) fn from_response(status: u16, body: Option<&[u8]>) -> Self {
        if (200..300).contains(&status) {
            let response =
                body.and_then(|body| serde_json::from_slice::<CommitTableResponse>(body).ok());
            return Self::Committed(Box::new(Committed {
                metadata_location: response.as_ref().map(|r| r.metadata_location.clone()),
                metadata: response.map(|r| r.metadata),
            }));
        }
        let model = body.and_then(ErrorEnvelope::decode);
        let kind = match status {
            409 => RejectionKind::RequirementFailed,
            400 => RejectionKind::BadRequest,
            401 | 419 => RejectionKind::Unauthorized,
            403 => RejectionKind::Forbidden,
            404 => RejectionKind::NotFound,
            406 => RejectionKind::Unsupported,
            _ => return Self::Unknown(UnknownOutcome::Status { status, model }),
        };
        Self::Rejected(CommitRejection {
            kind,
            status,
            model,
        })
    }
}

/// A commit whose authentication is already applied. Nothing has been sent.
///
/// Building one fetches any OAuth2 token, so a publisher can prepare it before
/// durably authorizing the send: token or URL failures then leave no authorized
/// request behind. Sending consumes it, so it is sent at most once.
pub struct AuthenticatedCommit<'a> {
    connection: &'a CatalogConnection,
    builder: reqwest::RequestBuilder,
}

impl fmt::Debug for AuthenticatedCommit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The builder carries the bearer token; never print it.
        f.debug_struct("AuthenticatedCommit")
            .finish_non_exhaustive()
    }
}

impl CatalogConnection {
    /// Builds an authenticated commit for `request`, with `idempotency_key`
    /// if given, without sending it.
    ///
    /// # Errors
    ///
    /// [`CatalogError::IdempotencyUnsupported`] when a key is given but the
    /// catalog does not advertise idempotency support,
    /// [`CatalogError::InvalidUrl`] for an unusable table name, and
    /// [`CatalogError::Authentication`] or [`CatalogError::Transport`] when
    /// no OAuth2 token could be fetched. Nothing has been sent in any case.
    pub async fn authenticate_commit(
        &self,
        request: &CommitRequest,
        idempotency_key: Option<IdempotencyKey>,
    ) -> Result<AuthenticatedCommit<'_>, CatalogError> {
        if idempotency_key.is_some() && self.capabilities().idempotency_key_lifetime.is_none() {
            return Err(CatalogError::IdempotencyUnsupported);
        }
        let table = request.table();
        let url = self.namespace_url(&table.namespace, &["tables", &table.name])?;
        let mut builder = self
            .client()
            .authorized(Method::POST, url)
            .await?
            .header(CONTENT_TYPE, "application/json")
            .body(request.body().to_vec());
        if let Some(key) = idempotency_key {
            builder = builder.header(IDEMPOTENCY_KEY, key.to_string());
        }
        Ok(AuthenticatedCommit {
            connection: self,
            builder,
        })
    }

    /// Sends `request` once, with `idempotency_key` if given.
    ///
    /// `Ok` means the request may have reached the catalog; the outcome
    /// says what is known. `Err` means it was not sent.
    ///
    /// # Errors
    ///
    /// The errors of [`authenticate_commit`](Self::authenticate_commit) and
    /// [`AuthenticatedCommit::send`].
    pub async fn commit(
        &self,
        request: &CommitRequest,
        idempotency_key: Option<IdempotencyKey>,
    ) -> Result<CommitOutcome, CatalogError> {
        self.authenticate_commit(request, idempotency_key)
            .await?
            .send()
            .await
    }
}

impl AuthenticatedCommit<'_> {
    /// Sends the commit once.
    ///
    /// `Ok` means the request may have reached the catalog; the outcome
    /// says what is known. An authentication rejection discards the cached
    /// OAuth2 token so that a later request fetches a fresh one.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Transport`] when no connection to the catalog could
    /// be established, so the request was not sent.
    pub async fn send(self) -> Result<CommitOutcome, CatalogError> {
        let response = match self.builder.send().await {
            Ok(response) => response,
            Err(source) if source.is_connect() => {
                return Err(CatalogError::Transport {
                    operation: OPERATION,
                    source,
                });
            }
            Err(source) if source.is_timeout() => {
                return Ok(CommitOutcome::Unknown(UnknownOutcome::Timeout));
            }
            Err(source) => return Ok(CommitOutcome::Unknown(UnknownOutcome::Transport(source))),
        };
        let status = response.status().as_u16();
        if matches!(status, 401 | 419) {
            self.connection.client().invalidate_token().await;
        }
        // The status is already conclusive; an unreadable body only loses detail.
        let body = response.bytes().await.ok();
        Ok(CommitOutcome::from_response(status, body.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFLICT: &[u8] =
        br#"{"error":{"message":"requirement failed","type":"CommitFailedException","code":409}}"#;

    fn rejection(status: u16) -> RejectionKind {
        match CommitOutcome::from_response(status, None) {
            CommitOutcome::Rejected(rejection) => rejection.kind,
            other => panic!("{status} is a rejection, got {other:?}"),
        }
    }

    #[test]
    fn success_statuses_are_committed_with_or_without_a_body() {
        for status in [200, 204] {
            assert!(matches!(
                CommitOutcome::from_response(status, Some(b"not json")),
                CommitOutcome::Committed(committed) if committed.metadata.is_none()
            ));
        }
        assert!(matches!(
            CommitOutcome::from_response(200, None),
            CommitOutcome::Committed(_)
        ));
    }

    #[test]
    fn specified_rejections_are_definitive() {
        assert_eq!(rejection(409), RejectionKind::RequirementFailed);
        assert_eq!(rejection(400), RejectionKind::BadRequest);
        assert_eq!(rejection(401), RejectionKind::Unauthorized);
        assert_eq!(rejection(419), RejectionKind::Unauthorized);
        assert_eq!(rejection(403), RejectionKind::Forbidden);
        assert_eq!(rejection(404), RejectionKind::NotFound);
        assert_eq!(rejection(406), RejectionKind::Unsupported);
    }

    #[test]
    fn rejections_keep_the_catalog_error() {
        let CommitOutcome::Rejected(rejection) = CommitOutcome::from_response(409, Some(CONFLICT))
        else {
            panic!("409 is a rejection");
        };

        assert_eq!(rejection.status, 409);
        assert_eq!(
            rejection.model,
            Some(CatalogErrorModel {
                kind: "CommitFailedException".to_owned(),
                message: "requirement failed".to_owned(),
            })
        );
    }

    #[test]
    fn every_other_status_is_unknown() {
        for status in [301, 307, 408, 429, 500, 502, 503, 504, 599] {
            assert!(
                matches!(
                    CommitOutcome::from_response(status, Some(CONFLICT)),
                    CommitOutcome::Unknown(UnknownOutcome::Status { status: s, model: Some(_) })
                        if s == status
                ),
                "{status}"
            );
        }
    }

    #[test]
    fn dispositions_follow_the_outcome() {
        let disposition = |status| CommitOutcome::from_response(status, None).disposition();

        assert_eq!(disposition(200), None);
        assert_eq!(disposition(409), Some(FailureDisposition::ReplanRequired));
        assert_eq!(disposition(404), Some(FailureDisposition::ReplanRequired));
        for status in [400, 401, 403, 406, 419] {
            assert_eq!(
                disposition(status),
                Some(FailureDisposition::PermanentFailure),
                "{status}"
            );
        }
        assert_eq!(
            disposition(500),
            Some(FailureDisposition::ResolvePublication)
        );
        assert_eq!(
            CommitOutcome::Unknown(UnknownOutcome::Timeout).disposition(),
            Some(FailureDisposition::ResolvePublication)
        );
    }

    #[test]
    fn idempotency_keys_round_trip() {
        let key = IdempotencyKey::generate();

        assert_eq!(IdempotencyKey::from_uuid(*key.as_uuid()), key);
        assert_eq!(key.as_uuid().get_version_num(), 7);
        assert_eq!(key.to_string(), key.as_uuid().to_string());
    }
}
