//! Non-secret worker configuration and signal handling.

use std::path::{Path, PathBuf};

use bergsmith_protocol::{cluster, identity::ServiceToken};
use bergsmith_storage::{S3Profile, StorageProfile};
use serde::Deserialize;

use super::{WorkerConfig, WorkerError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    endpoint: String,
    ca_certificate: Option<PathBuf>,
    worker_name: String,
    pool: String,
    capabilities: Vec<String>,
    memory_bytes: u64,
    cpu_millis: u32,
    state_directory: PathBuf,
    cgroup_root: Option<PathBuf>,
    s3_region: Option<String>,
    s3_endpoint: Option<String>,
    #[serde(default)]
    s3_path_style: bool,
    /// Hours to keep acknowledged journal records; 30 days by default.
    journal_retention_hours: Option<u64>,
}

/// Loads worker configuration, reads its token file, and handles termination.
///
/// The token file is read into memory only and is never sent to subprocesses.
/// A missing S3 region selects filesystem storage for development.
///
/// # Errors
/// Returns invalid JSON/configuration, file, transport, or runtime errors.
pub async fn serve(config_file: &Path, token_file: &Path) -> Result<(), WorkerError> {
    let config: ConfigFile = serde_json::from_slice(&tokio::fs::read(config_file).await?)
        .map_err(|_| WorkerError::Invalid("invalid worker configuration json"))?;
    let token = tokio::fs::read_to_string(token_file).await?;
    let token = ServiceToken::parse(token.trim())
        .map_err(|_| WorkerError::Invalid("invalid worker service token"))?;
    let ca_certificate = match config.ca_certificate {
        Some(path) => Some(tokio::fs::read(path).await?),
        None => None,
    };
    let storage = config.s3_region.map_or(StorageProfile::LocalFs, |region| {
        StorageProfile::S3(S3Profile {
            region,
            endpoint: config.s3_endpoint,
            path_style_access: config.s3_path_style,
        })
    });
    let config = WorkerConfig {
        endpoint: config.endpoint,
        ca_certificate,
        token,
        registration: cluster::RegisterWorkerRequest {
            worker_name: config.worker_name,
            pool: config.pool,
            protocol_revision: 2,
            capabilities: config.capabilities,
            capacity: Some(cluster::Capacity {
                max_concurrent_tasks: 1,
                memory_bytes: config.memory_bytes,
                cpu_millis: config.cpu_millis,
            }),
            build_version: env!("CARGO_PKG_VERSION").into(),
        },
        state_directory: config.state_directory,
        binary: std::env::current_exe()?,
        storage,
        cgroup_root: config.cgroup_root,
        journal_retention: config
            .journal_retention_hours
            .map_or(super::DEFAULT_JOURNAL_RETENTION, |hours| {
                std::time::Duration::from_secs(hours.saturating_mul(3600))
            }),
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let runtime = super::run(config, stopped);
    tokio::pin!(runtime);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = &mut runtime => return result,
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate.recv() => {},
    }
    let _ = stop.send(true);
    runtime.await
}
