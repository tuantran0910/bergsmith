//! Runtime-only `FileIO` confinement, quotas, deadlines and concurrency.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bergsmith_storage::Location;
use bytes::Bytes;
use futures::stream::BoxStream;
use iceberg::io::{
    FileIO, FileIOBuilder, FileMetadata, FileRead, FileWrite, InputFile, ListEntry, OutputFile,
    Storage, StorageConfig, StorageFactory,
};
use iceberg::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::{ExecutionLimits, ExecutorError, ResourceLimit};

pub(crate) struct IoState {
    base: FileIO,
    prefix: Location,
    limits: ExecutionLimits,
    permits: Semaphore,
    bytes: AtomicU64,
    read_bytes: AtomicU64,
    failure: AtomicU8,
    paths: Mutex<BTreeSet<String>>,
}

impl fmt::Debug for IoState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // FileIO configuration can contain credentials. Never derive Debug here.
        f.debug_struct("IoState").finish_non_exhaustive()
    }
}

impl IoState {
    pub(crate) fn new(base: FileIO, prefix: Location, limits: ExecutionLimits) -> Arc<Self> {
        Arc::new(Self {
            base,
            prefix,
            permits: Semaphore::new(limits.object_requests as usize),
            limits,
            bytes: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            failure: AtomicU8::new(0),
            paths: Mutex::new(BTreeSet::new()),
        })
    }

    pub(crate) fn file_io(self: &Arc<Self>) -> FileIO {
        FileIOBuilder::new(Arc::new(Guard {
            state: Some(self.clone()),
        }))
        .build()
    }

    pub(crate) fn error(&self, error: ExecutorError) -> ExecutorError {
        match self.failure.load(Ordering::Relaxed) {
            1 => ExecutorError::ResourceLimit(ResourceLimit::OutputBytes),
            2 => ExecutorError::OperationTimeout,
            3 => ExecutorError::ResourceLimit(ResourceLimit::OutputFiles),
            4 => ExecutorError::ResourceLimit(ResourceLimit::ReadBuffer),
            _ => error,
        }
    }

    pub(crate) fn bytes_written(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn fail(&self, code: u8) {
        // Keep the first failure: a later concurrent timeout must not turn a
        // deterministic resource failure into a retryable task failure.
        let _ = self
            .failure
            .compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
    }
    pub(crate) fn bytes_read(&self) -> u64 {
        self.read_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn paths(&self) -> Result<Vec<String>> {
        Ok(self
            .paths
            .lock()
            .map_err(|_| denied())?
            .iter()
            .cloned()
            .collect())
    }

    async fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        crate::deadline::within(Duration::from_millis(self.limits.io_timeout_ms), async {
            let _permit = self.permits.acquire().await.map_err(|_| denied())?;
            future.await
        })
        .await
        .ok_or_else(|| {
            self.fail(2);
            Error::new(ErrorKind::Unexpected, "task storage operation timed out")
                .with_retryable(true)
        })?
    }

    fn check_output(&self, path: &str) -> Result<()> {
        let location = Location::parse(path).map_err(|_| denied())?;
        if location == self.prefix || !location.is_within(&self.prefix) {
            return Err(denied());
        }
        Ok(())
    }

    fn reserve(&self, size: usize) -> Result<()> {
        self.bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bytes| {
                bytes
                    .checked_add(size as u64)
                    .filter(|sum| *sum <= self.limits.output_bytes)
            })
            .map_err(|_| {
                self.fail(1);
                Error::new(ErrorKind::DataInvalid, "task output byte limit exceeded")
            })?;
        Ok(())
    }
}

// iceberg's factory and storage traits require typetag. Only the type tag is
// serializable; deserialized guards have no runtime authority and fail closed.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Guard {
    #[serde(skip)]
    state: Option<Arc<IoState>>,
}

impl Guard {
    fn state(&self) -> Result<&Arc<IoState>> {
        self.state.as_ref().ok_or_else(denied)
    }
}

#[typetag::serde(name = "bergsmith-attempt-io-factory")]
impl StorageFactory for Guard {
    fn build(&self, _: &StorageConfig) -> Result<Arc<dyn Storage>> {
        self.state()?;
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
#[typetag::serde(name = "bergsmith-attempt-io")]
impl Storage for Guard {
    async fn exists(&self, path: &str) -> Result<bool> {
        let s = self.state()?;
        s.run(s.base.exists(path)).await
    }
    async fn metadata(&self, path: &str) -> Result<FileMetadata> {
        let s = self.state()?;
        s.run(s.base.new_input(path)?.metadata()).await
    }
    async fn read(&self, path: &str) -> Result<Bytes> {
        let s = self.state()?;
        let input = s.base.new_input(path)?;
        let metadata = s.run(input.metadata()).await?;
        if metadata.size > s.limits.read_request_bytes {
            s.fail(4);
            return Err(Error::new(
                ErrorKind::DataInvalid,
                "task metadata read limit exceeded",
            ));
        }
        let bytes = s.run(input.read()).await?;
        s.read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }
    async fn reader(&self, path: &str) -> Result<Box<dyn FileRead>> {
        let s = self.state()?;
        let inner = s.run(s.base.new_input(path)?.reader()).await?;
        Ok(Box::new(GuardRead {
            inner,
            state: s.clone(),
        }))
    }
    async fn write(&self, path: &str, bytes: Bytes) -> Result<()> {
        let mut writer = self.writer(path).await?;
        writer.write(bytes).await?;
        writer.close().await
    }
    async fn writer(&self, path: &str) -> Result<Box<dyn FileWrite>> {
        let s = self.state()?;
        s.check_output(path)?;
        {
            let mut paths = s.paths.lock().map_err(|_| denied())?;
            if paths.len() >= s.limits.output_files {
                s.fail(3);
                return Err(Error::new(
                    ErrorKind::DataInvalid,
                    "task output file limit exceeded",
                ));
            }
            if !paths.insert(path.to_owned()) {
                return Err(denied());
            }
        }
        if s.run(s.base.exists(path)).await? {
            return Err(denied());
        }
        let inner = s.run(s.base.new_output(path)?.writer()).await?;
        Ok(Box::new(GuardWrite {
            inner,
            state: s.clone(),
        }))
    }
    async fn delete(&self, _: &str) -> Result<()> {
        Err(denied())
    }
    async fn delete_prefix(&self, _: &str) -> Result<()> {
        Err(denied())
    }
    async fn delete_stream(&self, _: BoxStream<'static, String>) -> Result<()> {
        Err(denied())
    }
    async fn list(&self, _: &str, _: bool) -> Result<BoxStream<'static, Result<ListEntry>>> {
        Err(denied())
    }
    fn new_input(&self, path: &str) -> Result<InputFile> {
        self.state()?;
        Ok(InputFile::new(Arc::new(self.clone()), path.to_owned()))
    }
    fn new_output(&self, path: &str) -> Result<OutputFile> {
        self.state()?.check_output(path)?;
        Ok(OutputFile::new(Arc::new(self.clone()), path.to_owned()))
    }
}

struct GuardRead {
    inner: Box<dyn FileRead>,
    state: Arc<IoState>,
}

#[async_trait]
impl FileRead for GuardRead {
    async fn read(&self, range: Range<u64>) -> Result<Bytes> {
        if range.end.saturating_sub(range.start) > self.state.limits.read_request_bytes {
            self.state.fail(4);
            return Err(Error::new(
                ErrorKind::DataInvalid,
                "task range read limit exceeded",
            ));
        }
        let bytes = self.state.run(self.inner.read(range)).await?;
        self.state
            .read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }
}

struct GuardWrite {
    inner: Box<dyn FileWrite>,
    state: Arc<IoState>,
}

#[async_trait]
impl FileWrite for GuardWrite {
    async fn write(&mut self, bytes: Bytes) -> Result<()> {
        self.state.reserve(bytes.len())?;
        self.state.run(self.inner.write(bytes)).await
    }
    async fn close(&mut self) -> Result<()> {
        self.state.run(self.inner.close()).await
    }
}

fn denied() -> Error {
    Error::new(
        ErrorKind::DataInvalid,
        "task storage operation is not authorized",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    fn state(limits: ExecutionLimits) -> Arc<IoState> {
        IoState::new(
            FileIO::new_with_memory(),
            Location::parse("memory://test/attempt").unwrap(),
            limits,
        )
    }

    #[tokio::test]
    async fn only_new_children_of_the_attempt_can_be_written() {
        let s = state(ExecutionLimits::default());
        let io = s.file_io();
        assert!(io.new_output("memory://test/source.parquet").is_err());
        assert!(
            io.new_output("memory://test/attempt-other/p.parquet")
                .is_err()
        );
        assert!(io.new_output("memory://test/attempt").is_err());
        io.new_output("memory://test/attempt/p.parquet")
            .unwrap()
            .write(Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert_eq!(s.bytes_written(), 4);
        assert!(
            io.new_output("memory://test/attempt/p.parquet")
                .unwrap()
                .writer()
                .await
                .is_err()
        );
        assert!(io.delete("memory://test/attempt/p.parquet").await.is_err());
        assert!(io.delete_prefix("memory://test/attempt").await.is_err());
        assert!(
            io.delete_stream(Box::pin(futures::stream::iter([
                "memory://test/source.parquet".to_owned()
            ])))
            .await
            .is_err()
        );
        assert!(io.list("memory://test/attempt/", true).await.is_err());
        assert_eq!(
            io.new_input("memory://test/attempt/p.parquet")
                .unwrap()
                .read()
                .await
                .unwrap()
                .as_ref(),
            b"data"
        );
    }

    #[tokio::test]
    async fn output_quota_is_reserved_before_storage_receives_bytes() {
        let s = state(ExecutionLimits {
            output_bytes: 3,
            ..ExecutionLimits::default()
        });
        let io = s.file_io();
        let mut writer = io
            .new_output("memory://test/attempt/p")
            .unwrap()
            .writer()
            .await
            .unwrap();
        assert!(writer.write(Bytes::from_static(b"four")).await.is_err());
        assert_eq!(s.bytes_written(), 0);
        assert!(matches!(
            s.error(ExecutorError::InvalidLimits),
            ExecutorError::ResourceLimit(ResourceLimit::OutputBytes)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn operation_deadlines_include_waiting_for_an_io_permit() {
        let s = state(ExecutionLimits {
            object_requests: 1,
            ..ExecutionLimits::default()
        });
        let _permit = s.permits.acquire().await.unwrap();
        assert!(s.run(async { Ok(()) }).await.is_err());
        assert!(matches!(
            s.error(ExecutorError::InvalidLimits),
            ExecutorError::OperationTimeout
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_timeout_cannot_make_an_output_limit_failure_retryable() {
        let s = state(ExecutionLimits {
            output_bytes: 1,
            ..ExecutionLimits::default()
        });
        assert!(s.reserve(2).is_err());
        assert!(s.run(std::future::pending::<Result<()>>()).await.is_err());
        let error = s.error(ExecutorError::InvalidLimits);
        assert!(matches!(
            error,
            ExecutorError::ResourceLimit(ResourceLimit::OutputBytes)
        ));
        assert_eq!(
            error.disposition(),
            bergsmith_common::disposition::FailureDisposition::PermanentFailure
        );
    }

    #[test]
    fn serialization_cannot_transfer_runtime_storage_authority() {
        let guard = Guard {
            state: Some(state(ExecutionLimits::default())),
        };
        let value = serde_json::to_value(&guard).unwrap();
        assert_eq!(value, serde_json::json!({}));
        let restored: Guard = serde_json::from_value(value).unwrap();
        assert!(StorageFactory::build(&restored, &StorageConfig::default()).is_err());
        assert!(restored.new_input("memory://test/source").is_err());
    }

    #[tokio::test]
    async fn existing_storage_objects_are_not_overwritten_by_a_fresh_guard() {
        let s = state(ExecutionLimits::default());
        s.base
            .new_output("memory://test/attempt/p")
            .unwrap()
            .write(Bytes::from_static(b"original"))
            .await
            .unwrap();
        assert!(
            s.file_io()
                .new_output("memory://test/attempt/p")
                .unwrap()
                .writer()
                .await
                .is_err()
        );
        assert_eq!(
            s.base
                .new_input("memory://test/attempt/p")
                .unwrap()
                .read()
                .await
                .unwrap()
                .as_ref(),
            b"original"
        );
        let entries: Vec<_> = s
            .base
            .list("memory://test/attempt/", true)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
    }
}
