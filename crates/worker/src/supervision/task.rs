//! Same-build native task client. It has no coordinator or catalog token.

use std::path::Path;
use std::time::{Duration, SystemTime};

use bergsmith_common::{digest::Sha256Digest, ids::AttemptId};
use bergsmith_maintenance::{
    AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor, NativeExecutor,
};
use bergsmith_protocol::{cluster, local};
use bergsmith_storage::{
    Backend, CredentialCell, Location, ObjectStore, S3Profile, StorageCredential, StorageProfile,
};
use tonic::transport::{Channel, Endpoint};

use super::{RPC_TIMEOUT, WorkerError, credentials_expired, deadline, duration, failure};

pub(super) async fn connect(
    socket: &Path,
) -> Result<local::local_task_service_client::LocalTaskServiceClient<Channel>, WorkerError> {
    let socket = socket.to_owned();
    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_timeout(RPC_TIMEOUT)
        .timeout(RPC_TIMEOUT)
        .connect_with_connector(tower::service_fn(move |_| {
            let socket = socket.clone();
            async move {
                tokio::net::UnixStream::connect(socket)
                    .await
                    .map(hyper_util::rt::TokioIo::new)
            }
        }))
        .await?;
    Ok(
        local::local_task_service_client::LocalTaskServiceClient::new(channel)
            .max_decoding_message_size(super::MAX_MESSAGE)
            .max_encoding_message_size(super::MAX_MESSAGE),
    )
}

pub(super) fn credential(
    value: &cluster::StorageCredentials,
) -> Result<StorageCredential, WorkerError> {
    let mut credential = StorageCredential::from_vended(
        &value.prefix,
        &value
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
    .map_err(|_| WorkerError::Invalid("invalid vended credentials"))?;
    if let Some(expiry) = value.expires_at {
        credential.expires_at = Some(deadline(Some(expiry))?);
    }
    Ok(credential)
}

pub(super) fn store(
    location: &Location,
    region: &str,
    endpoint: Option<String>,
    path_style_access: bool,
    credentials: Option<&cluster::StorageCredentials>,
    cell: &CredentialCell,
) -> Result<ObjectStore, WorkerError> {
    let profile = match location.backend() {
        Backend::LocalFs => StorageProfile::LocalFs,
        Backend::S3 => {
            let value = credentials.ok_or(WorkerError::Invalid("missing storage credentials"))?;
            let value = credential(value)?;
            if !value.covers(location) || !value.valid_for(SystemTime::now(), Duration::ZERO) {
                return Err(WorkerError::Invalid("credentials expired or out of scope"));
            }
            cell.replace(vec![value]);
            StorageProfile::S3(S3Profile {
                region: region.to_owned(),
                endpoint,
                path_style_access,
            })
        }
        _ => return Err(WorkerError::Invalid("unsupported worker storage")),
    };
    ObjectStore::open(&profile, Some(cell.clone()))
        .map_err(|_| WorkerError::Invalid("storage could not be opened"))
}

/// Runs the task client over its private Unix socket.
///
/// Credentials stay in memory and are replaced on the shared `FileIO` provider.
/// Cancellation or loss of the control stream stops execution. The supervisor
/// enforces process termination for native work that cannot be cancelled.
///
/// # Errors
/// Returns a typed transport, configuration, or execution error. No result is
/// produced after credential expiry or cancellation.
pub async fn execute(socket: &Path) -> Result<(), WorkerError> {
    let mut client = connect(socket).await?;
    let mut request = tonic::Request::new(local::GetTaskRequest {});
    request.set_timeout(RPC_TIMEOUT);
    let task = client.get_task(request).await?.into_inner();
    let limits = task
        .limits
        .as_ref()
        .ok_or(WorkerError::Invalid("missing limits"))?;
    super::process::apply_limits(limits)?;
    let mut request = tonic::Request::new(local::WatchControlRequest {});
    request.set_timeout(Duration::from_secs(7 * 24 * 3600));
    let mut control = client.watch_control(request).await?.into_inner();
    let credentials = CredentialCell::default();
    let mut current_credentials = task.credentials.clone();
    let artifact = task
        .artifact
        .as_ref()
        .ok_or(WorkerError::Invalid("missing artifact"))?;
    let table = artifact
        .table
        .as_ref()
        .ok_or(WorkerError::Invalid("missing table"))?;
    let location = Location::parse(&table.location)
        .map_err(|_| WorkerError::Invalid("invalid table location"))?;
    let storage = store(
        &location,
        &task.s3_region,
        task.s3_endpoint.clone(),
        task.s3_path_style,
        task.credentials.as_ref(),
        &credentials,
    )?;
    let execution = native(&task, &storage);
    tokio::pin!(execution);
    let mut expiry_check = tokio::time::interval(Duration::from_millis(100));
    let outcome = loop {
        tokio::select! {
            biased;
            message = control.message() => {
                let Some(message) = message? else { break Err(failure("TaskCancelled", false)); };
                match message.control {
                    Some(local::watch_control_response::Control::RefreshedCredentials(value)) => {
                        let replacement = credential(&value)?;
                        if !replacement.covers(&location) || !replacement.valid_for(SystemTime::now(), Duration::ZERO) {
                            break Err(failure("StorageAuthorizationExpired", true));
                        }
                        credentials.replace(vec![replacement]);
                        current_credentials = Some(value);
                    }
                    Some(local::watch_control_response::Control::Cancel(_)) => break Err(failure("TaskCancelled", false)),
                    None => {},
                }
            }
            _ = expiry_check.tick() => {
                if credentials_expired(current_credentials.as_ref()) { break Err(failure("StorageAuthorizationExpired", true)); }
            }
            result = &mut execution => break result.map_err(|error| super::worker_failure(&error, current_credentials.as_ref())),
        }
    };
    match outcome {
        Ok(receipt) => {
            if credentials_expired(current_credentials.as_ref()) {
                return Err(WorkerError::Invalid("storage credentials expired"));
            }
            let metrics = receipt
                .metrics
                .as_ref()
                .ok_or(WorkerError::Invalid("missing execution metrics"))?;
            let mut progress =
                tonic::Request::new(tokio_stream::iter([local::ReportProgressRequest {
                    progress: Some(cluster::AttemptProgress {
                        rows_read: metrics.rows_read,
                        rows_written: metrics.rows_written,
                        bytes_written: metrics.bytes_written,
                        output_paths: receipt
                            .outputs
                            .iter()
                            .map(|output| output.path.clone())
                            .collect(),
                    }),
                }]));
            progress.set_timeout(RPC_TIMEOUT);
            let mut request = tonic::Request::new(local::ReportResultRequest {
                receipt: Some(receipt),
            });
            request.set_timeout(RPC_TIMEOUT);
            client.report_result(request).await?;
            // Completion is durable before optional diagnostics can delay exit.
            let _ = client.report_progress(progress).await;
        }
        Err(failure) => {
            let mut request = tonic::Request::new(local::ReportFailureRequest {
                failure: Some(failure),
            });
            request.set_timeout(RPC_TIMEOUT);
            client.report_failure(request).await?;
        }
    }
    Ok(())
}

async fn native(
    task: &local::GetTaskResponse,
    storage: &ObjectStore,
) -> Result<bergsmith_protocol::artifact::TaskReceipt, WorkerError> {
    let artifact = task
        .artifact
        .as_ref()
        .ok_or(WorkerError::Invalid("missing artifact"))?;
    let limits = task
        .limits
        .as_ref()
        .ok_or(WorkerError::Invalid("missing limits"))?;
    let input = storage
        .file_io()
        .new_input(&task.metadata_location)
        .map_err(bergsmith_maintenance::ExecutorError::from)?;
    let bytes = tokio::time::timeout(RPC_TIMEOUT, async {
        if input.metadata().await?.size > super::MAX_MESSAGE as u64 {
            return Err(bergsmith_maintenance::ExecutorError::InvalidData(
                "oversized pinned metadata",
            ));
        }
        input
            .read()
            .await
            .map_err(bergsmith_maintenance::ExecutorError::from)
    })
    .await
    .map_err(|_| WorkerError::Timeout)??;
    let metadata = serde_json::from_slice(&bytes)
        .map_err(|_| WorkerError::Invalid("invalid pinned metadata"))?;
    let memory = limits.memory_bytes;
    let execution_limits = ExecutionLimits {
        batch_memory_bytes: memory.min(32 * 1024 * 1024),
        delete_memory_bytes: memory.min(256 * 1024 * 1024),
        decode_row_group_bytes: memory.min(128 * 1024 * 1024),
        read_request_bytes: memory.min(128 * 1024 * 1024),
        output_bytes: limits.max_output_bytes,
        duration_ms: u64::try_from(duration(limits.max_duration)?.as_millis())
            .map_err(|_| WorkerError::Invalid("duration overflow"))?,
        ..ExecutionLimits::default()
    };
    let output = Location::parse(&task.output_prefix)
        .map_err(|_| WorkerError::Invalid("invalid output prefix"))?;
    let attempt_id: AttemptId = task
        .attempt_id
        .parse()
        .map_err(|_| WorkerError::Invalid("invalid attempt id"))?;
    let result = NativeExecutor
        .execute(ExecutionRequest {
            input: ExecutionInput {
                artifact,
                artifact_sha256: Sha256Digest::from_slice(&task.artifact_sha256)
                    .map_err(|_| WorkerError::Invalid("invalid artifact digest"))?,
                metadata: &metadata,
                metadata_location: &task.metadata_location,
                file_io: storage.file_io(),
            },
            attempt: AttemptContext {
                attempt_id,
                generation: task.generation,
            },
            output_prefix: &output,
            limits: execution_limits,
        })
        .await?;
    Ok(result.receipt().clone())
}
