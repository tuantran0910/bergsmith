//! W3C trace context between a worker and the coordinator (ADR 0068).
//!
//! Only authenticated worker RPCs continue a remote trace. Browser and API
//! requests always start a new trace, and Bergsmith never sends its trace
//! context to a catalog, object store, or notification destination.

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The W3C headers that carry `span`'s context, as lowercase name and value
/// pairs, for the worker to attach to a coordinator RPC. The list is empty
/// when trace export is off or `span` is not recorded.
#[must_use]
pub fn trace_headers(span: &tracing::Span) -> Vec<(String, String)> {
    let context = span.context();
    if !context.span().span_context().is_valid() {
        return Vec::new();
    }
    let mut headers = Headers(Vec::new());
    TraceContextPropagator::new().inject_context(&context, &mut headers);
    headers.0
}

/// A worker's trace context, extracted from an RPC's headers and held until
/// the caller is authenticated.
///
/// Spans start lazily (ADR 0068), so the coordinator's `worker_rpc` span has
/// no trace ID or sampling decision until it is started. Adopting the parent
/// only after the worker's token is verified means an unauthenticated caller
/// can neither choose the trace ID nor force sampling; its spans start new
/// traces.
#[derive(Clone, Debug)]
pub struct RemoteParent {
    span: tracing::Span,
    context: opentelemetry::Context,
}

impl RemoteParent {
    /// The W3C context in `headers`, to become `span`'s parent later; `None`
    /// when the headers carry no valid context.
    #[must_use]
    pub fn extract(span: &tracing::Span, headers: &http::HeaderMap) -> Option<Self> {
        let context = TraceContextPropagator::new().extract(&HeaderExtractor(headers));
        context.span().span_context().is_valid().then(|| Self {
            span: span.clone(),
            context,
        })
    }

    /// Makes the extracted context the span's parent. Call it only once the
    /// caller is authenticated. It has no effect once the span has started,
    /// for example because a child span started first, or when trace export
    /// is off; the span then stays a root.
    pub fn adopt(&self) {
        let _ = self.span.set_parent(self.context.clone());
    }
}

struct Headers(Vec<(String, String)>);

impl Injector for Headers {
    fn set(&mut self, key: &str, value: String) {
        self.0.push((key.to_ascii_lowercase(), value));
    }
}

struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}
