//! The OTLP/gRPC span exporter and its tracer provider (ADR 0068).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry_otlp::tonic_types::transport::{Certificate, ClientTlsConfig};
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::resource::EnvResourceDetector;
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider, SpanData, SpanExporter};

use crate::TelemetryError;
use crate::sampler::BoundedSampler;
use crate::settings::TraceSettings;

/// Builds the tracer provider for `service`. Must run inside a Tokio
/// runtime, which drives the exporter's gRPC channel.
///
/// # Errors
///
/// [`TelemetryError::Certificate`] when the CA bundle cannot be read, and
/// [`TelemetryError::Exporter`] when the exporter rejects its settings.
pub(crate) fn provider(
    service: &'static str,
    settings: &TraceSettings,
) -> Result<SdkTracerProvider, TelemetryError> {
    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(settings.endpoint.clone());
    if settings.endpoint.starts_with("https://") {
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(path) = &settings.certificate {
            let pem = std::fs::read(path).map_err(|error| TelemetryError::Certificate {
                path: path.clone(),
                reason: error.to_string(),
            })?;
            tls = tls.ca_certificate(Certificate::from_pem(pem));
        }
        builder = builder.with_tls_config(tls);
    }
    let exporter = builder.build().map_err(|_| TelemetryError::Exporter)?;
    Ok(SdkTracerProvider::builder()
        .with_span_processor(BatchSpanProcessor::builder(Reporting::new(exporter)).build())
        .with_sampler(BoundedSampler::from(settings.sampler))
        .with_resource(resource(service, settings.service_name.as_deref()))
        .build())
}

/// `service.name` follows the specification's precedence:
/// `OTEL_SERVICE_NAME`, then `OTEL_RESOURCE_ATTRIBUTES`, then the binary.
fn resource(service: &'static str, service_name: Option<&str>) -> Resource {
    let mut builder = Resource::builder_empty()
        .with_service_name(service)
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_detector(Box::new(EnvResourceDetector::new()));
    if let Some(name) = service_name {
        builder = builder.with_service_name(name.to_owned());
    }
    builder.build()
}

/// Reports export failures in the log once per outage rather than per
/// batch, and never includes the request's headers.
pub(crate) struct Reporting<E> {
    inner: E,
    failing: AtomicBool,
}

impl<E> Reporting<E> {
    pub(crate) fn new(inner: E) -> Self {
        Self {
            inner,
            failing: AtomicBool::new(false),
        }
    }

    fn record(&self, result: &OTelSdkResult) {
        match result {
            Ok(()) => {
                if self.failing.swap(false, Ordering::Relaxed) {
                    tracing::info!("trace export recovered");
                }
            }
            Err(error) => {
                if !self.failing.swap(true, Ordering::Relaxed) {
                    tracing::warn!(error = %error, "trace export failed; spans are dropped until it recovers");
                }
            }
        }
    }
}

impl<E: fmt::Debug> fmt::Debug for Reporting<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Reporting")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<E: SpanExporter> SpanExporter for Reporting<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.record(&result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::Key;
    use opentelemetry_sdk::error::OTelSdkError;

    use super::*;

    #[derive(Debug)]
    struct Failing;

    impl SpanExporter for Failing {
        async fn export(&self, _: Vec<SpanData>) -> OTelSdkResult {
            Err(OTelSdkError::InternalFailure("unavailable".into()))
        }
    }

    #[tokio::test]
    async fn a_failing_exporter_reports_its_failure() {
        let reporting = Reporting::new(Failing);
        assert!(reporting.export(Vec::new()).await.is_err());
        assert!(reporting.failing.load(Ordering::Relaxed));
        reporting.record(&Ok(()));
        assert!(!reporting.failing.load(Ordering::Relaxed));
    }

    #[test]
    fn the_service_name_follows_the_specified_precedence() {
        let name = |resource: Resource| {
            resource
                .get(&Key::new("service.name"))
                .map(|value| value.to_string())
        };
        assert_eq!(
            name(resource("bergsmith-worker", None)).as_deref(),
            Some("bergsmith-worker")
        );
        assert_eq!(
            name(resource("bergsmith-worker", Some("pool-a"))).as_deref(),
            Some("pool-a")
        );
    }
}
