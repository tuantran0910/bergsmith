//! Pull-based worker runtime and its private, same-build task transport.

mod journal;
mod process;
mod runtime;
mod service;
mod startup;
mod task;

#[cfg(target_os = "linux")]
pub use process::bootstrap;
pub use process::prepare_subprocess;
pub use runtime::{WorkerConfig, run};
pub use startup::serve;
pub use task::execute;

#[cfg(test)]
mod tests;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bergsmith_protocol::{cluster, common};

/// A worker configuration, transport, journal, or subprocess failure.
/// Error messages intentionally exclude network and storage credential details.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Invalid or incompatible configuration or grant.
    #[error("invalid worker configuration or task grant: {0}")]
    Invalid(&'static str),
    /// Local state or process operation failed.
    #[error("worker local operation failed ({kind})", kind = .0.kind())]
    Io(#[from] std::io::Error),
    /// An RPC could not complete; the caller retains its immutable request.
    #[error("worker rpc failed")]
    Rpc(#[from] tonic::Status),
    /// Connection to the configured peer failed.
    #[error("worker transport failed")]
    Transport(#[from] tonic::transport::Error),
    /// The stored result or artifact is malformed.
    #[error("worker protobuf is invalid")]
    Decode(#[from] prost::DecodeError),
    /// An operation exceeded its deadline.
    #[error("worker operation timed out")]
    Timeout,
    /// A task could not be prepared or executed.
    #[error("worker native execution failed")]
    Execution(#[from] bergsmith_maintenance::ExecutorError),
}

pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const MAX_MESSAGE: usize = 16 * 1024 * 1024;

pub(super) async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, WorkerError> + Send + 'static,
) -> Result<T, WorkerError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| WorkerError::Invalid("worker blocking operation failed"))?
}

pub(super) fn duration(value: Option<prost_types::Duration>) -> Result<Duration, WorkerError> {
    let value = value.ok_or(WorkerError::Invalid("missing duration"))?;
    if value.seconds < 0 || !(0..1_000_000_000).contains(&value.nanos) {
        return Err(WorkerError::Invalid("invalid duration"));
    }
    let result = Duration::new(value.seconds.unsigned_abs(), value.nanos.unsigned_abs());
    if result.is_zero() || result > Duration::from_secs(7 * 24 * 3600) {
        return Err(WorkerError::Invalid("duration outside supported bounds"));
    }
    Ok(result)
}

pub(super) fn deadline(value: Option<prost_types::Timestamp>) -> Result<SystemTime, WorkerError> {
    let value = value.ok_or(WorkerError::Invalid("missing deadline"))?;
    if value.seconds < 0 || !(0..1_000_000_000).contains(&value.nanos) {
        return Err(WorkerError::Invalid("invalid deadline"));
    }
    UNIX_EPOCH
        .checked_add(Duration::new(
            value.seconds.unsigned_abs(),
            value.nanos.unsigned_abs(),
        ))
        .ok_or(WorkerError::Invalid("deadline overflow"))
}

pub(super) fn failure(reason: &'static str, retry: bool) -> common::Failure {
    common::Failure {
        reason_code: reason.into(),
        disposition: if retry {
            common::FailureDisposition::RetryTask
        } else {
            common::FailureDisposition::PermanentFailure
        }
        .into(),
        message: "supervised task stopped without a publishable result".into(),
    }
}

pub(super) fn worker_failure(
    error: &WorkerError,
    credentials: Option<&cluster::StorageCredentials>,
) -> common::Failure {
    if credentials_expired(credentials) {
        return failure("StorageAuthorizationExpired", true);
    }
    match error {
        WorkerError::Execution(error) => error.to_failure(),
        WorkerError::Timeout => failure("StorageOperationTimedOut", true),
        WorkerError::Rpc(_) | WorkerError::Transport(_) => failure("TaskSubprocessCrashed", true),
        _ => failure("InvalidTaskGrant", false),
    }
}

pub(super) fn credentials_expired(credentials: Option<&cluster::StorageCredentials>) -> bool {
    credentials
        .and_then(|c| c.expires_at)
        .is_some_and(|expiry| {
            deadline(Some(expiry)).map_or(true, |expiry| expiry <= SystemTime::now())
        })
}
