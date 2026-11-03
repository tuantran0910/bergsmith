//! OTLP export against an in-process collector (ADR 0068). Nextest runs
//! each test in its own process, so each installs its own global subscriber.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test setup and lookups fail the test"
)]

use std::time::{Duration, Instant};

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::trace::v1::Span;

/// The spans a collector received, with their resource's `service.name`.
#[derive(Clone, Default)]
pub(crate) struct Received(Arc<Mutex<Vec<(String, Span)>>>);

impl Received {
    pub(crate) fn spans(&self) -> Vec<(String, Span)> {
        self.0.lock().unwrap().clone()
    }
}

#[tonic::async_trait]
impl TraceService for Received {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        let mut spans = self.0.lock().unwrap();
        for resource in request.into_inner().resource_spans {
            let service = resource
                .resource
                .iter()
                .flat_map(|resource| &resource.attributes)
                .find(|attribute| attribute.key == "service.name")
                .and_then(|attribute| attribute.value.as_ref())
                .map(|value| format!("{:?}", value.value))
                .unwrap_or_default();
            for scope in resource.scope_spans {
                spans.extend(scope.spans.into_iter().map(|span| (service.clone(), span)));
            }
        }
        Ok(tonic::Response::new(ExportTraceServiceResponse::default()))
    }
}

/// Starts a collector on a free local port.
pub(crate) async fn collector() -> (SocketAddr, Received) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let received = Received::default();
    let service = TraceServiceServer::new(received.clone());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (address, received)
}

/// A lookup over fixed variables, in place of the process environment.
pub(crate) fn variables(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

fn named<'a>(spans: &'a [(String, Span)], name: &str) -> &'a Span {
    &spans
        .iter()
        .find(|(_, span)| span.name == name)
        .unwrap_or_else(|| panic!("no span named {name} in {spans:?}"))
        .1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_trace_continues_on_the_coordinator() {
    let (address, received) = collector().await;
    let telemetry = bergsmith_telemetry::install_from(
        "bergsmith-test",
        variables(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}")),
            ("OTEL_SERVICE_NAME", "telemetry-test".to_owned()),
        ]),
    )
    .unwrap();
    assert!(telemetry.exports_traces());

    let mut headers = http::HeaderMap::new();
    {
        let attempt =
            tracing::info_span!(target: "bergsmith_worker", "task_attempt", attempt_id = "a-1");
        let _entered = attempt.enter();
        let pairs = bergsmith_telemetry::trace_headers(&attempt);
        assert!(
            pairs.iter().any(|(name, _)| name == "traceparent"),
            "{pairs:?}"
        );
        for (name, value) in pairs {
            headers.insert(
                http::HeaderName::try_from(name).unwrap(),
                http::HeaderValue::try_from(value).unwrap(),
            );
        }
    }
    {
        // An authenticated RPC: the span is entered before authentication
        // finishes, and adopts the worker's context afterwards.
        let rpc = tracing::info_span!(target: "bergsmith_coordinator", "worker_rpc", method = "Heartbeat");
        let parent = bergsmith_telemetry::RemoteParent::extract(&rpc, &headers).unwrap();
        let _entered = rpc.enter();
        tracing::info!(target: "bergsmith_coordinator", "inside the rpc");
        parent.adopt();
        let child = tracing::info_span!(target: "bergsmith_coordinator", "load_session");
        let _child = child.enter();
        // A dependency's span, such as a SQL statement, is never exported.
        let query =
            tracing::info_span!(target: "sqlx::query", "statement", db.statement = "select 1");
        let _query = query.enter();
    }
    {
        // An RPC that fails authentication never adopts: a new trace, so the
        // caller can neither pick the trace ID nor force sampling.
        let rpc = tracing::info_span!(target: "bergsmith_coordinator", "rejected_rpc");
        let _parent = bergsmith_telemetry::RemoteParent::extract(&rpc, &headers).unwrap();
        let _entered = rpc.enter();
    }
    {
        // A request without trace headers has nothing to adopt.
        let rpc = tracing::info_span!(target: "bergsmith_coordinator", "http_request");
        assert!(
            bergsmith_telemetry::RemoteParent::extract(&rpc, &http::HeaderMap::new()).is_none()
        );
        let _entered = rpc.enter();
    }

    telemetry.shutdown().await;
    let spans = received.spans();
    assert_eq!(spans.len(), 5, "{spans:?}");
    assert!(
        spans
            .iter()
            .all(|(service, _)| service.contains("telemetry-test"))
    );
    let attempt = named(&spans, "task_attempt");
    let rpc = named(&spans, "worker_rpc");
    let child = named(&spans, "load_session");
    let rejected = named(&spans, "rejected_rpc");
    let request = named(&spans, "http_request");
    assert_eq!(rpc.trace_id, attempt.trace_id);
    assert_eq!(rpc.parent_span_id, attempt.span_id);
    assert_eq!(child.trace_id, attempt.trace_id);
    assert_eq!(child.parent_span_id, rpc.span_id);
    assert_eq!(rpc.events.len(), 1, "{:?}", rpc.events);
    for root in [rejected, request] {
        assert_ne!(root.trace_id, attempt.trace_id);
        assert!(root.parent_span_id.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_collector_does_not_hold_up_the_process() {
    // Reserve a port, then close it, so nothing listens there.
    let address = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let telemetry = bergsmith_telemetry::install_from(
        "bergsmith-test",
        // The exporter's own timeout is read from the process environment,
        // so this relies on the default; the shutdown bound is what is tested.
        variables(&[("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))]),
    )
    .unwrap();
    let started = Instant::now();
    for index in 0..10_000 {
        let span = tracing::info_span!(target: "bergsmith_test", "work", index);
        let _entered = span.enter();
    }
    assert!(started.elapsed() < Duration::from_secs(5), "spans blocked");

    let started = Instant::now();
    telemetry.shutdown().await;
    assert!(
        started.elapsed() < bergsmith_telemetry::SHUTDOWN_TIMEOUT + Duration::from_secs(2),
        "shutdown took {:?}",
        started.elapsed()
    );
}

#[test]
fn trace_export_outside_a_runtime_is_refused() {
    let result = bergsmith_telemetry::install_from(
        "bergsmith-test",
        variables(&[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://127.0.0.1:1".to_owned(),
        )]),
    );
    assert!(matches!(
        result,
        Err(bergsmith_telemetry::TelemetryError::NoRuntime)
    ));
}
