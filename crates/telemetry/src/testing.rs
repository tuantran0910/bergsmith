//! Span capture for other crates' tests (the `testing` feature).
//!
//! [`capture`] installs a thread-local subscriber with the same Bergsmith-only
//! trace filter as production export, and records finished spans in memory
//! instead of sending them to a collector.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use tracing_subscriber::Layer as _;
use tracing_subscriber::prelude::*;

/// One finished span, with IDs as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedSpan {
    /// The span's name after any `otel.name` override.
    pub name: String,
    /// The trace it belongs to.
    pub trace_id: String,
    /// Its own ID.
    pub span_id: String,
    /// Its parent's ID, or all zeros for a root.
    pub parent_span_id: String,
    /// Attributes by key, with values in their display form.
    pub attributes: BTreeMap<String, String>,
}

/// Spans captured since [`capture`].
#[derive(Debug)]
pub struct Captured {
    provider: SdkTracerProvider,
    spans: Arc<Mutex<Vec<SpanData>>>,
}

impl Captured {
    /// The spans that have finished so far, in the order they ended.
    #[must_use]
    pub fn spans(&self) -> Vec<CapturedSpan> {
        let _ = self.provider.force_flush();
        self.spans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|span| CapturedSpan {
                name: span.name.to_string(),
                trace_id: span.span_context.trace_id().to_string(),
                span_id: span.span_context.span_id().to_string(),
                parent_span_id: span.parent_span_id.to_string(),
                attributes: span
                    .attributes
                    .iter()
                    .map(|pair| (pair.key.to_string(), pair.value.to_string()))
                    .collect(),
            })
            .collect()
    }
}

/// Captures Bergsmith spans on this thread until the guard drops.
#[must_use]
pub fn capture() -> (tracing::subscriber::DefaultGuard, Captured) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(Memory(Arc::clone(&spans)))
        .build();
    let filter = crate::trace_filter(crate::settings::DEFAULT_TRACE_FILTER)
        .unwrap_or_else(|_| unreachable!("the default trace filter parses"));
    let subscriber = tracing_subscriber::Registry::default()
        .with(crate::exporting_layer(provider.tracer("bergsmith-test")).with_filter(filter));
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, Captured { provider, spans })
}

#[derive(Debug)]
struct Memory(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Memory {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(batch);
        Ok(())
    }
}
