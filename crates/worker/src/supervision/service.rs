//! Private local RPC server with immutable completion and bounded progress.

use std::pin::Pin;
use std::sync::Arc;

use bergsmith_protocol::{cluster, common, local};
use futures::{Stream, StreamExt};
use tokio::sync::{Mutex, watch};
use tonic::{Request, Response, Status};

use super::{WorkerError, journal};

#[derive(Clone)]
pub(super) struct LocalService {
    pub(super) task: Arc<Mutex<local::GetTaskResponse>>,
    pub(super) control: watch::Sender<Control>,
    pub(super) progress: Arc<Mutex<cluster::AttemptProgress>>,
    pub(super) completion: Arc<Mutex<Option<Completion>>>,
    pub(super) directory: std::path::PathBuf,
    pub(super) session: String,
    pub(super) journal: journal::Journal,
}

#[derive(Clone, Default)]
pub(super) struct Control {
    pub(super) cancel: Option<cluster::Cancel>,
    pub(super) credentials: Option<cluster::StorageCredentials>,
}

#[derive(Clone)]
pub(super) enum Completion {
    Result,
    Failure,
}

impl LocalService {
    pub(super) async fn fail(&self, failure: common::Failure) -> Result<(), WorkerError> {
        let mut completed = self.completion.clone().lock_owned().await;
        if completed.is_some() {
            return Ok(());
        }
        let task = self.task.lock().await;
        let report = cluster::ReportTaskFailureRequest {
            worker_session_id: self.session.clone(),
            attempt_id: task.attempt_id.clone(),
            generation: task.generation,
            failure: Some(failure.clone()),
        };
        drop(task);
        let path = self.directory.join("failure.pb");
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || {
            let result = journal.record(&path, &report);
            if path.exists() {
                *completed = Some(Completion::Failure);
            }
            result
        })
        .await
        .map_err(|_| WorkerError::Invalid("journal task failed"))??;
        Ok(())
    }
}

#[tonic::async_trait]
impl local::local_task_service_server::LocalTaskService for LocalService {
    async fn get_task(
        &self,
        _: Request<local::GetTaskRequest>,
    ) -> Result<Response<local::GetTaskResponse>, Status> {
        Ok(Response::new(self.task.lock().await.clone()))
    }

    async fn report_progress(
        &self,
        request: Request<tonic::Streaming<local::ReportProgressRequest>>,
    ) -> Result<Response<local::ReportProgressResponse>, Status> {
        let mut stream = request.into_inner();
        while let Some(message) = stream.next().await {
            let progress = message?
                .progress
                .ok_or(Status::invalid_argument("missing progress"))?;
            let task = self.task.lock().await;
            let root = bergsmith_storage::Location::parse(&task.output_prefix)
                .map_err(|_| Status::internal("invalid output prefix"))?;
            if progress.rows_read < 0
                || progress.rows_written < 0
                || progress.bytes_written < 0
                || progress.output_paths.len() > 10_000
                || progress.output_paths.iter().any(|path| {
                    bergsmith_storage::Location::parse(path)
                        .map_or(true, |path| !path.is_within(&root))
                })
            {
                return Err(Status::invalid_argument("invalid task progress"));
            }
            *self.progress.lock().await = progress;
        }
        Ok(Response::new(local::ReportProgressResponse {}))
    }

    type WatchControlStream =
        Pin<Box<dyn Stream<Item = Result<local::WatchControlResponse, Status>> + Send>>;

    async fn watch_control(
        &self,
        _: Request<local::WatchControlRequest>,
    ) -> Result<Response<Self::WatchControlStream>, Status> {
        // Each snapshot carries the latest credentials and sticky cancellation.
        // A lagging client cannot lose either kind of update.
        let stream = tokio_stream::wrappers::WatchStream::new(self.control.subscribe()).flat_map(
            |control| {
                let mut messages = Vec::new();
                if let Some(credentials) = control.credentials {
                    messages.push(Ok(local::WatchControlResponse {
                        control: Some(
                            local::watch_control_response::Control::RefreshedCredentials(
                                credentials,
                            ),
                        ),
                    }));
                }
                if let Some(cancel) = control.cancel {
                    messages.push(Ok(local::WatchControlResponse {
                        control: Some(local::watch_control_response::Control::Cancel(cancel)),
                    }));
                }
                futures::stream::iter(messages)
            },
        );
        let ready =
            futures::stream::once(async { Ok(local::WatchControlResponse { control: None }) });
        Ok(Response::new(Box::pin(ready.chain(stream))))
    }

    async fn report_result(
        &self,
        request: Request<local::ReportResultRequest>,
    ) -> Result<Response<local::ReportResultResponse>, Status> {
        let receipt = request
            .into_inner()
            .receipt
            .ok_or(Status::invalid_argument("missing receipt"))?;
        let mut completed = self.completion.clone().lock_owned().await;
        if matches!(*completed, Some(Completion::Failure)) {
            return Err(Status::failed_precondition("task already failed"));
        }
        if completed.is_none() && self.control.borrow().cancel.is_some() {
            return Err(Status::cancelled("task cancelled"));
        }
        let task = self.task.lock().await;
        if receipt.attempt_id != task.attempt_id
            || receipt.generation != task.generation
            || receipt.task_id
                != task
                    .artifact
                    .as_ref()
                    .map_or("", |artifact| artifact.task_id.as_str())
            || receipt.artifact_sha256 != task.artifact_sha256
        {
            return Err(Status::invalid_argument("receipt identity mismatch"));
        }
        let report = cluster::ReportTaskResultRequest {
            worker_session_id: self.session.clone(),
            receipt_sha256: bergsmith_protocol::hashing::receipt_digest(&receipt)
                .as_bytes()
                .to_vec(),
            receipt: Some(receipt),
        };
        drop(task);
        let path = self.directory.join("result.pb");
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || {
            let result = journal.record(&path, &report);
            if path.exists() {
                *completed = Some(Completion::Result);
            }
            result
        })
        .await
        .map_err(|_| Status::internal("receipt journal task failed"))?
        .map_err(|_| Status::internal("receipt could not be persisted"))?;
        Ok(Response::new(local::ReportResultResponse {}))
    }

    async fn report_failure(
        &self,
        request: Request<local::ReportFailureRequest>,
    ) -> Result<Response<local::ReportFailureResponse>, Status> {
        self.fail(
            request
                .into_inner()
                .failure
                .ok_or(Status::invalid_argument("missing failure"))?,
        )
        .await
        .map_err(|_| Status::internal("failure could not be persisted"))?;
        Ok(Response::new(local::ReportFailureResponse {}))
    }
}
