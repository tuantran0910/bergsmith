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
    /// Invalid or incompatible configuration, or a worker-local fault.
    #[error("invalid worker configuration or local state: {0}")]
    Invalid(&'static str),
    /// The coordinator's grant cannot be executed by any worker as issued.
    /// Reported as the permanent `InvalidTaskGrant`.
    #[error("invalid task grant: {0}")]
    InvalidGrant(&'static str),
    /// The fetched task artifact did not decode or verify against its grant.
    /// The coordinator verified it before granting, so this retries the task.
    #[error("task artifact could not be verified: {0}")]
    UnverifiedArtifact(&'static str),
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

/// Maps a supervisor or task-client error to a stable typed failure.
///
/// Worker-local infrastructure faults (socket, spawn, cgroup, journal and
/// other local I/O) are the retryable `WorkerLocalFailure`; only a grant that
/// cannot be executed as issued is the permanent `InvalidTaskGrant`.
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
        WorkerError::InvalidGrant(_) => failure("InvalidTaskGrant", false),
        WorkerError::UnverifiedArtifact(_) => failure("TaskArtifactUnverified", true),
        WorkerError::Invalid(_) | WorkerError::Io(_) | WorkerError::Decode(_) => {
            failure("WorkerLocalFailure", true)
        }
    }
}

/// Converts a coordinator lease deadline into a local monotonic deadline.
///
/// `coordinator_now` is the coordinator's HTTP `Date` response header, so the
/// remaining lease is measured entirely on the coordinator's clock and worker
/// wall-clock skew cannot shorten or extend it. The header has one-second
/// resolution, so one second is subtracted. Without the header, the worker's
/// wall clock is the fallback reference.
#[must_use]
pub(super) fn local_deadline(
    expiry: SystemTime,
    coordinator_now: Option<SystemTime>,
    local_now: SystemTime,
    received: tokio::time::Instant,
) -> tokio::time::Instant {
    let remaining = match coordinator_now {
        Some(now) => expiry
            .duration_since(now)
            .unwrap_or_default()
            .saturating_sub(Duration::from_secs(1)),
        None => expiry.duration_since(local_now).unwrap_or_default(),
    };
    received + remaining
}

/// Parses an IMF-fixdate HTTP `Date` value such as
/// `Sun, 06 Nov 1994 08:49:37 GMT`. Returns `None` for any other form.
#[must_use]
pub(super) fn http_date(value: &str) -> Option<SystemTime> {
    let field = |start: usize, end: usize| value.get(start..end);
    let number = |start: usize, end: usize| -> Option<u64> {
        let text = field(start, end)?;
        if text.bytes().all(|byte| byte.is_ascii_digit()) {
            text.parse().ok()
        } else {
            None
        }
    };
    if value.len() != 29
        || field(3, 5)? != ", "
        || field(7, 8)? != " "
        || field(11, 12)? != " "
        || field(16, 17)? != " "
        || field(19, 20)? != ":"
        || field(22, 23)? != ":"
        || field(25, 29)? != " GMT"
    {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| field(8, 11) == Some(*name))?;
    let month = u64::try_from(month).ok()? + 1;
    let (day, year) = (number(5, 7)?, number(12, 16)?);
    let (hour, minute, second) = (number(17, 19)?, number(20, 22)?, number(23, 25)?);
    if !(1..=31).contains(&day) || year < 1970 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since the epoch from a civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted / 400;
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    UNIX_EPOCH.checked_add(Duration::from_secs(
        days * 86_400 + hour * 3600 + minute * 60 + second,
    ))
}

/// Returns the coordinator's response time from gRPC response metadata.
pub(super) fn coordinator_time(metadata: &tonic::metadata::MetadataMap) -> Option<SystemTime> {
    http_date(metadata.get("date")?.to_str().ok()?)
}

/// Whether vended storage credentials have expired by the worker's wall clock.
///
/// The catalog issues credentials for the storage service, and request signing
/// uses this same clock, so the coordinator's clock is not a better reference
/// here (unlike attempt leases).
pub(super) fn credentials_expired(credentials: Option<&cluster::StorageCredentials>) -> bool {
    credentials
        .and_then(|c| c.expires_at)
        .is_some_and(|expiry| {
            deadline(Some(expiry)).map_or(true, |expiry| expiry <= SystemTime::now())
        })
}
