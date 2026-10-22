//! Worker RPC latency: a tower layer on the worker gRPC server.
//!
//! Each call is timed from request to response headers and recorded by
//! method and gRPC status code; a unary handler has finished by then. The
//! call also runs inside a `worker_rpc` span, so its log events carry the
//! method.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http;
use tower::{Layer, Service};
use tracing::Instrument;

use super::exposition::{Exposition, Kind};

/// Upper bounds of the latency buckets, in seconds. Long polls end at the
/// coordinator's long-poll timeout, so the top buckets cover it.
const BUCKETS: [f64; 13] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

const FAMILY: &str = "bergsmith_worker_rpc_duration_seconds";

#[derive(Clone, Debug, Default)]
struct Histogram {
    /// Non-cumulative counts per bucket; the last slot is `+Inf`.
    buckets: [u64; BUCKETS.len() + 1],
    count: u64,
    sum: f64,
}

/// Process-local worker RPC latency histograms, keyed by method and code.
#[derive(Clone, Debug, Default)]
pub struct RpcMetrics {
    histograms: Arc<Mutex<BTreeMap<(String, String), Histogram>>>,
}

impl RpcMetrics {
    /// Records one call.
    pub(crate) fn observe(&self, method: &str, code: &str, seconds: f64) {
        let mut histograms = self
            .histograms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let histogram = histograms
            .entry((method.to_owned(), code.to_owned()))
            .or_default();
        let slot = BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(BUCKETS.len());
        histogram.buckets[slot] += 1;
        histogram.count += 1;
        histogram.sum += seconds;
    }

    /// Writes the histogram family.
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts stay far below 2^53 within a process lifetime"
    )]
    pub(crate) fn write(&self, out: &mut Exposition) {
        let histograms = self
            .histograms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        out.family(
            FAMILY,
            Kind::Histogram,
            "Worker RPC latency on this coordinator, by method and gRPC code.",
        );
        let bucket = format!("{FAMILY}_bucket");
        let sum = format!("{FAMILY}_sum");
        let count = format!("{FAMILY}_count");
        for ((method, code), histogram) in &histograms {
            let mut cumulative = 0;
            for (index, observed) in histogram.buckets.iter().enumerate() {
                cumulative += observed;
                let bound = BUCKETS
                    .get(index)
                    .map_or_else(|| "+Inf".to_owned(), ToString::to_string);
                out.sample(
                    &bucket,
                    &[("method", method), ("code", code), ("le", &bound)],
                    cumulative as f64,
                );
            }
            let labels = [("method", method.as_str()), ("code", code.as_str())];
            out.sample(&sum, &labels, histogram.sum);
            out.sample(&count, &labels, histogram.count as f64);
        }
    }

    /// The layer that records into these histograms.
    #[must_use]
    pub fn layer(&self) -> RpcLatencyLayer {
        RpcLatencyLayer {
            metrics: self.clone(),
        }
    }
}

/// Times every worker RPC; see [`RpcMetrics::layer`].
#[derive(Clone, Debug)]
pub struct RpcLatencyLayer {
    metrics: RpcMetrics,
}

impl<S> Layer<S> for RpcLatencyLayer {
    type Service = RpcLatency<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcLatency {
            inner,
            metrics: self.metrics.clone(),
        }
    }
}

/// The service produced by [`RpcLatencyLayer`].
#[derive(Clone, Debug)]
pub struct RpcLatency<S> {
    inner: S,
    metrics: RpcMetrics,
}

impl<S, B, R> Service<http::Request<B>> for RpcLatency<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let method = method(request.uri().path());
        let span = tracing::info_span!("worker_rpc", method);
        let started = Instant::now();
        let call = self.inner.call(request);
        let metrics = self.metrics.clone();
        Box::pin(
            async move {
                let result = call.await;
                let code = match &result {
                    Ok(response) => code(response.headers()),
                    Err(_) => "Transport".to_owned(),
                };
                metrics.observe(method, &code, started.elapsed().as_secs_f64());
                result
            }
            .instrument(span),
        )
    }
}

/// The RPC name from `/package.Service/Method`; anything else is `unknown`,
/// so arbitrary paths cannot create label values.
fn method(path: &str) -> &'static str {
    const METHODS: [&str; 7] = [
        "RegisterWorker",
        "RequestTask",
        "Heartbeat",
        "ReportTaskResult",
        "ReportTaskFailure",
        "GetAttemptStatus",
        "DrainWorker",
    ];
    let name = path
        .strip_prefix("/bergsmith.cluster.v1.ClusterService/")
        .unwrap_or_default();
    METHODS
        .into_iter()
        .find(|method| *method == name)
        .unwrap_or("unknown")
}

/// The status of a trailers-only (error) response, or `Ok`: a successful
/// unary response carries its status in trailers.
fn code(headers: &http::HeaderMap) -> String {
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i32>().ok())
        .map_or_else(
            || "Ok".to_owned(),
            |value| format!("{:?}", tonic::Code::from_i32(value)),
        )
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use tower::ServiceExt;

    use super::*;

    #[test]
    fn methods_are_bounded_to_the_cluster_service() {
        assert_eq!(
            method("/bergsmith.cluster.v1.ClusterService/Heartbeat"),
            "Heartbeat"
        );
        assert_eq!(
            method("/bergsmith.cluster.v1.ClusterService/Nope"),
            "unknown"
        );
        assert_eq!(method("/other.Service/Heartbeat"), "unknown");
    }

    #[test]
    fn histograms_are_cumulative_with_sum_and_count() {
        let metrics = RpcMetrics::default();
        metrics.observe("Heartbeat", "Ok", 0.003);
        metrics.observe("Heartbeat", "Ok", 0.2);
        metrics.observe("Heartbeat", "Ok", 120.0);
        let mut out = Exposition::default();

        metrics.write(&mut out);
        let text = out.finish();

        let line = |le: &str| {
            format!(
                "bergsmith_worker_rpc_duration_seconds_bucket{{method=\"Heartbeat\",code=\"Ok\",le=\"{le}\"}}"
            )
        };
        assert!(text.contains(&format!("{} 1\n", line("0.005"))));
        assert!(text.contains(&format!("{} 1\n", line("0.1"))));
        assert!(text.contains(&format!("{} 2\n", line("0.25"))));
        assert!(text.contains(&format!("{} 2\n", line("60"))));
        assert!(text.contains(&format!("{} 3\n", line("+Inf"))));
        assert!(text.contains(
            "bergsmith_worker_rpc_duration_seconds_count{method=\"Heartbeat\",code=\"Ok\"} 3\n"
        ));
        assert!(text.contains(
            "bergsmith_worker_rpc_duration_seconds_sum{method=\"Heartbeat\",code=\"Ok\"} 120.203\n"
        ));
    }

    #[tokio::test]
    async fn the_layer_records_the_method_and_trailers_only_status() {
        let metrics = RpcMetrics::default();
        let service =
            metrics
                .layer()
                .layer(tower::service_fn(|request: http::Request<()>| async move {
                    let mut response = http::Response::new(());
                    if request.uri().path().ends_with("Heartbeat") {
                        response
                            .headers_mut()
                            .insert("grpc-status", http::HeaderValue::from_static("5"));
                    }
                    Ok::<_, Infallible>(response)
                }));
        for path in [
            "/bergsmith.cluster.v1.ClusterService/Heartbeat",
            "/bergsmith.cluster.v1.ClusterService/RequestTask",
        ] {
            let request = http::Request::builder().uri(path).body(()).unwrap();
            service.clone().oneshot(request).await.unwrap();
        }
        let mut out = Exposition::default();

        metrics.write(&mut out);
        let text = out.finish();

        assert!(text.contains("{method=\"Heartbeat\",code=\"NotFound\"} 1\n"));
        assert!(text.contains("{method=\"RequestTask\",code=\"Ok\"} 1\n"));
    }
}
