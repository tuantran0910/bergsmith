//! Immutable reporting records; credentials never enter these files.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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

/// Directory, inside the state directory, that holds records being removed.
///
/// Retention first renames an attempt directory here, so a crash during the
/// recursive delete never leaves a partial record in the journal.
pub(super) const TRASH: &str = ".trash";

/// Removes acknowledged records whose acknowledgment is older than
/// `retention` at `now`, and finishes any removal a crash interrupted.
///
/// Only records with `ack.pb` and without `quarantine.pb` are removed:
/// unacknowledged reports must still be delivered, and quarantined ones are
/// kept for inspection. An acknowledgment follows the coordinator's definitive
/// decision on an ended attempt, and the coordinator never grants an ended
/// attempt again, so a removed record can never be needed to prevent a second
/// execution. Returns the number of records removed.
///
/// # Errors
/// Returns local I/O errors; records already removed stay removed, and the
/// next pass resumes from the trash directory.
pub(super) fn prune(
    root: &Path,
    retention: Duration,
    now: SystemTime,
) -> Result<usize, WorkerError> {
    let trash = root.join(TRASH);
    if !trash.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(&trash)?;
    }
    empty(&trash)?;
    let mut removed = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let directory = entry.path();
        if directory.join(QUARANTINE).exists() {
            continue;
        }
        let acknowledged = match std::fs::metadata(directory.join("ack.pb")) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if now.duration_since(acknowledged).unwrap_or_default() < retention {
            continue;
        }
        std::fs::rename(&directory, trash.join(entry.file_name()))?;
        removed += 1;
    }
    if removed > 0 {
        // The renames are durable before any content disappears.
        std::fs::File::open(root)?.sync_all()?;
        empty(&trash)?;
    }
    Ok(removed)
}

fn empty(trash: &Path) -> Result<(), WorkerError> {
    for entry in std::fs::read_dir(trash)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
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
            // Hidden directories, such as the retention trash, hold no records.
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.')
            {
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

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn record(root: &Path, name: &str, files: &[&str], age: Duration) -> PathBuf {
        let directory = root.join(name);
        private_dir(&directory).unwrap();
        for file in files {
            let path = directory.join(file);
            std::fs::write(&path, b"record").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::now().checked_sub(age).unwrap())
                .unwrap();
        }
        directory
    }

    fn state() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worker");
        private_dir(&root).unwrap();
        (directory, root)
    }

    #[test]
    fn only_expired_acknowledged_records_are_removed() {
        let (_directory, root) = state();
        let expired = record(&root, "expired", &["result.pb", "ack.pb"], 8 * DAY);
        let failed = record(&root, "failed", &["failure.pb", "ack.pb"], 8 * DAY);
        let recent = record(&root, "recent", &["result.pb", "ack.pb"], DAY);
        let pending = record(&root, "pending", &["result.pb"], 30 * DAY);
        let quarantined = record(
            &root,
            "quarantined",
            &["result.pb", "ack.pb", QUARANTINE],
            30 * DAY,
        );
        std::fs::write(root.join("worker.lock"), b"").unwrap();

        let removed = prune(&root, 7 * DAY, SystemTime::now()).unwrap();

        assert_eq!(removed, 2);
        assert!(!expired.exists());
        assert!(!failed.exists());
        assert!(recent.join("ack.pb").exists());
        assert!(pending.join("result.pb").exists());
        assert!(quarantined.join(QUARANTINE).exists());
        assert!(root.join("worker.lock").exists());
        assert_eq!(std::fs::read_dir(root.join(TRASH)).unwrap().count(), 0);
        assert_eq!(
            Journal::open(&root).unwrap().pending().unwrap(),
            vec![pending.join("result.pb")]
        );
    }

    #[test]
    fn an_interrupted_removal_finishes_and_never_reaches_the_journal() {
        let (_directory, root) = state();
        // A crash after the rename leaves a partial record in the trash.
        let interrupted = root.join(TRASH).join("interrupted");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&interrupted)
            .unwrap();
        std::fs::write(interrupted.join("result.pb"), b"partial").unwrap();
        std::fs::write(root.join(TRASH).join("stray"), b"").unwrap();

        assert!(Journal::open(&root).unwrap().pending().unwrap().is_empty());
        assert_eq!(prune(&root, 7 * DAY, SystemTime::now()).unwrap(), 0);

        assert_eq!(std::fs::read_dir(root.join(TRASH)).unwrap().count(), 0);
    }

    #[test]
    fn retention_is_measured_from_the_acknowledgment() {
        let (_directory, root) = state();
        // An old report acknowledged recently is still within retention.
        let directory = record(&root, "late-ack", &["result.pb"], 30 * DAY);
        record(&root, "late-ack", &["ack.pb"], DAY);

        assert_eq!(prune(&root, 7 * DAY, SystemTime::now()).unwrap(), 0);
        assert_eq!(
            prune(&root, 7 * DAY, SystemTime::now() + 7 * DAY).unwrap(),
            1
        );
        assert!(!directory.exists());
    }
}
