//! Immutable reporting records; credentials never enter these files.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bergsmith_protocol::cluster;
use prost::Message;

use super::{MAX_MESSAGE, WorkerError};

pub(super) fn private_dir(path: &Path) -> Result<(), WorkerError> {
    if !path.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(WorkerError::Invalid("state directory must be private"));
    }
    Ok(())
}

/// Marker recording why an unacknowledgeable record was set aside.
pub(super) const QUARANTINE: &str = "quarantine.pb";

/// Whether an attempt directory's record is acknowledged or quarantined.
pub(super) fn settled(directory: &Path) -> bool {
    directory.join("ack.pb").exists() || directory.join(QUARANTINE).exists()
}

pub(super) fn lock(path: &Path) -> Result<std::fs::File, WorkerError> {
    private_dir(path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path.join("worker.lock"))?;
    file.try_lock()
        .map_err(|_| WorkerError::Invalid("worker state directory is already in use"))?;
    Ok(file)
}

/// Atomically publishes a synced, immutable record without replacing an older one.
pub(super) fn persist<M: Message>(path: &Path, message: &M) -> Result<(), WorkerError> {
    let bytes = message.encode_to_vec();
    if bytes.len() > MAX_MESSAGE {
        return Err(WorkerError::Invalid("report exceeds message limit"));
    }
    let parent = path
        .parent()
        .ok_or(WorkerError::Invalid("missing report directory"))?;
    if path.exists() {
        if std::fs::read(path)? == bytes {
            // A prior writer may have published the record but failed syncing
            // its directory. An identical replay must finish that durability.
            std::fs::File::open(path)?.sync_all()?;
            std::fs::File::open(parent)?.sync_all()?;
            return Ok(());
        }
        return Err(WorkerError::Invalid("conflicting immutable report"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Clone)]
pub(super) struct Journal {
    pending: Arc<Mutex<BTreeSet<PathBuf>>>,
}

impl Journal {
    /// Rebuilds the pending index once, before polling or completion writers start.
    pub(super) fn open(directory: &Path) -> Result<Self, WorkerError> {
        let recovered = Self::scan(directory)?;
        Ok(Self {
            pending: Arc::new(Mutex::new(recovered.into_iter().collect())),
        })
    }
    pub(super) fn pending(&self) -> Result<Vec<PathBuf>, WorkerError> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| WorkerError::Invalid("journal index lock failed"))?
            .iter()
            .cloned()
            .collect())
    }
    fn scan(root: &Path) -> Result<Vec<PathBuf>, WorkerError> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let directory = entry.path();
            if settled(&directory) {
                continue;
            }
            let result = directory.join("result.pb").exists();
            let failure = directory.join("failure.pb").exists();
            if result && failure {
                return Err(WorkerError::Invalid("conflicting completion records"));
            }
            for name in ["result.pb", "failure.pb"] {
                let path = directory.join(name);
                if path.exists() {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        Ok(paths)
    }

    /// Called by the completion writer while its cancellation-safe guard is held.
    pub(super) fn record<M: Message>(&self, path: &Path, message: &M) -> Result<(), WorkerError> {
        let result = persist(path, message);
        // Even a failed directory fsync can leave a published immutable record.
        // Index it and preserve its outcome rather than permit an opposite report.
        if path.exists() {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| WorkerError::Invalid("journal index lock failed"))?;
            if path.parent().is_some_and(|directory| !settled(directory)) {
                pending.insert(path.to_owned());
            }
        }
        result
    }

    /// Durably sets aside a record the coordinator will never acknowledge.
    ///
    /// The record and its `quarantine.pb` reason stay for inspection; recovery
    /// and polling treat the attempt as settled, so it never blocks the worker.
    pub(super) fn quarantine(
        &self,
        path: &Path,
        reason: &bergsmith_protocol::common::Failure,
    ) -> Result<(), WorkerError> {
        persist(&path.with_file_name(QUARANTINE), reason)?;
        self.acknowledged(path)
    }

    pub(super) fn acknowledged(&self, path: &Path) -> Result<(), WorkerError> {
        self.pending
            .lock()
            .map_err(|_| WorkerError::Invalid("journal index lock failed"))?
            .remove(path);
        Ok(())
    }

    pub(super) fn result(path: &Path) -> Result<cluster::ReportTaskResultRequest, WorkerError> {
        let bytes = std::fs::read(path)?;
        if bytes.len() > MAX_MESSAGE {
            return Err(WorkerError::Invalid("oversized journal record"));
        }
        let report = cluster::ReportTaskResultRequest::decode(bytes.as_slice())?;
        let receipt = report
            .receipt
            .as_ref()
            .ok_or(WorkerError::Invalid("missing receipt"))?;
        if report.receipt_sha256 != bergsmith_protocol::hashing::receipt_digest(receipt).as_bytes()
        {
            return Err(WorkerError::Invalid("corrupt receipt digest"));
        }
        Ok(report)
    }
}
