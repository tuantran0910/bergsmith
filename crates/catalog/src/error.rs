//! Errors from catalog calls.

use std::fmt;

use bergsmith_storage::{CredentialError, LocationError};

/// The error body an Iceberg REST catalog returns, without the stack trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogErrorModel {
    /// The error type, such as `NoSuchTableException`.
    pub kind: String,
    /// The catalog's message.
    pub message: String,
}

impl fmt::Display for CatalogErrorModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// The standard REST error response.
#[derive(serde::Deserialize)]
pub(crate) struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

impl ErrorEnvelope {
    /// The error model in `body`, if it is a standard error response.
    pub(crate) fn decode(body: &[u8]) -> Option<CatalogErrorModel> {
        serde_json::from_slice::<Self>(body)
            .ok()
            .map(|envelope| CatalogErrorModel {
                kind: envelope.error.kind,
                message: envelope.error.message,
            })
    }
}

/// An error from a catalog call. Messages never include credentials.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The configured URI or a path built from it is invalid.
    #[error("invalid catalog URL: {0}")]
    InvalidUrl(String),
    /// The request could not be sent or the response not received, including
    /// timeouts. Whether a mutation took effect is unknown.
    #[error("catalog request to {operation} failed in transport")]
    Transport {
        /// The operation, such as `GET /v1/config`.
        operation: &'static str,
        /// The underlying error.
        #[source]
        source: reqwest::Error,
    },
    /// The catalog answered with an error status.
    #[error("catalog {operation} returned HTTP {status}{}", .model.as_ref().map(|m| format!(" ({m})")).unwrap_or_default())]
    Status {
        /// The operation.
        operation: &'static str,
        /// The HTTP status code.
        status: u16,
        /// The decoded error body, when there was one.
        model: Option<CatalogErrorModel>,
    },
    /// The response body did not decode.
    #[error("catalog {operation} returned an undecodable body")]
    Decode {
        /// The operation.
        operation: &'static str,
        /// The decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// The response body was larger than the call allows.
    #[error("catalog {operation} returned more than {max_bytes} bytes")]
    ResponseTooLarge {
        /// The operation.
        operation: &'static str,
        /// The byte limit the call applies.
        max_bytes: usize,
    },
    /// The OAuth2 token endpoint refused the client credentials.
    #[error("OAuth2 token request failed with HTTP {status}")]
    Authentication {
        /// The HTTP status code.
        status: u16,
    },
    /// The catalog lacks endpoints Bergsmith needs.
    #[error("catalog does not support required endpoints: {}", .0.join(", "))]
    MissingEndpoints(Vec<String>),
    /// A table location from the catalog is not a valid storage location.
    #[error("catalog returned an invalid table location")]
    InvalidLocation(#[source] LocationError),
    /// Vended credentials could not be used.
    #[error("catalog vended unusable storage credentials")]
    Credentials(#[source] CredentialError),
    /// The catalog vended no credentials for a table loaded with delegation.
    #[error("catalog vended no storage credentials for the table")]
    NoVendedCredentials,
    /// The table's data location is outside its table location.
    #[error("table data location {0} is outside the table location")]
    DataLocationOutsideTable(String),
    /// An idempotency key was given for a catalog that does not advertise
    /// `idempotency-key-lifetime`.
    #[error("catalog does not support idempotency keys")]
    IdempotencyUnsupported,
}

impl CatalogError {
    /// Whether the catalog reported that the table or namespace does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Status { status: 404, .. })
    }
}
