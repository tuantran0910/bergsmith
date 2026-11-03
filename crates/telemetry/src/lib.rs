//! Log and trace installation for Bergsmith binaries (ADRs 0044 and 0068).
//!
//! [`install`] reads the process environment once:
//!
//! - `BERGSMITH_LOG_FORMAT` selects `text` (the default) or `json` lines,
//!   which include the active span and its parents; `RUST_LOG` filters them.
//! - Spans are exported over OTLP/gRPC only when
//!   `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT` is
//!   set, unless `OTEL_SDK_DISABLED=true` or `OTEL_TRACES_EXPORTER=none`.
//!   Only Bergsmith's own spans and events are exported, filtered by
//!   `BERGSMITH_TRACE_FILTER` (default `info`); dependency spans, such as
//!   SQL statements and the exporter's own requests, never leave the process.
//!
//! [`trace_headers`] and [`RemoteParent`] carry a worker's trace context to
//! the coordinator's worker RPCs.

mod export;
mod propagation;
mod sampler;
mod settings;
#[cfg(feature = "testing")]
pub mod testing;

use std::path::PathBuf;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::filter::{EnvFilter, FilterExt, filter_fn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, Registry};

pub use propagation::{RemoteParent, trace_headers};

use settings::{LogFormat, Settings, TraceSettings};

/// How long [`Telemetry::shutdown`] waits for queued spans to be exported.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Why telemetry could not be installed as configured. None of the messages
/// repeat a variable's value, because endpoint and header variables may
/// carry credentials.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// An environment variable has an unsupported value.
    #[error("{name} {reason}")]
    Setting {
        /// The variable's name.
        name: &'static str,
        /// What a valid value looks like.
        reason: &'static str,
    },
    /// A filter directive could not be parsed.
    #[error("{name} is not a valid filter: {reason}")]
    Filter {
        /// The variable's name.
        name: &'static str,
        /// The parser's explanation.
        reason: String,
    },
    /// The collector's CA bundle could not be read.
    #[error("trace collector certificate {} could not be read: {reason}", path.display())]
    Certificate {
        /// The configured path.
        path: PathBuf,
        /// The I/O error.
        reason: String,
    },
    /// The OTLP exporter rejected its settings. The exporter's own message is
    /// not kept, because it can repeat the endpoint.
    #[error("trace exporter could not be built from the OTEL_EXPORTER_OTLP_* settings")]
    Exporter,
    /// Trace export needs a Tokio runtime to drive its gRPC channel.
    #[error("trace export must be installed inside a Tokio runtime")]
    NoRuntime,
    /// Another subscriber was installed first.
    #[error("a log subscriber is already installed")]
    AlreadyInstalled,
}

/// The installed telemetry. Call [`Telemetry::shutdown`] before the process
/// exits so queued spans are exported.
#[derive(Debug)]
#[must_use = "dropping the handle without shutdown can lose queued spans"]
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    /// Whether spans are being exported.
    #[must_use]
    pub fn exports_traces(&self) -> bool {
        self.provider.is_some()
    }

    /// Exports queued spans, waiting at most [`SHUTDOWN_TIMEOUT`], and stops
    /// the exporter. A failure is logged, not returned: telemetry never
    /// changes how a process exits.
    pub async fn shutdown(self) {
        let Some(provider) = self.provider else {
            return;
        };
        // The batch processor joins its thread, so stay off async threads.
        let result =
            tokio::task::spawn_blocking(move || provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT))
                .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(error = %error, "trace export did not finish"),
            Err(error) => tracing::warn!(error = %error, "trace export did not finish"),
        }
    }
}

/// Installs the global log subscriber and, when configured, OTLP trace
/// export for the binary named `service`.
///
/// # Errors
///
/// Any [`TelemetryError`]. Plain text logging is installed before an error
/// is returned, so the caller can log it and exit.
pub fn install(service: &'static str) -> Result<Telemetry, TelemetryError> {
    install_from(service, |name| std::env::var(name).ok())
}

/// [`install`] with Bergsmith's settings read through `lookup` instead of
/// the process environment; `lookup` returns a value or `None` when unset.
/// The exporter's own variables (headers, timeout, compression, `OTEL_BSP_*`)
/// and `OTEL_RESOURCE_ATTRIBUTES` are still read from the process
/// environment by the OpenTelemetry crates.
///
/// # Errors
///
/// As for [`install`].
pub fn install_from(
    service: &'static str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Telemetry, TelemetryError> {
    let result = Settings::read(lookup).and_then(|settings| {
        let provider = match &settings.trace {
            Some(trace) => Some(build_provider(service, trace)?),
            None => None,
        };
        subscriber(&settings, service, provider.as_ref())?;
        Ok(Telemetry { provider })
    });
    if result
        .as_ref()
        .is_err_and(|error| !matches!(error, TelemetryError::AlreadyInstalled))
    {
        install_fallback();
    }
    result
}

/// Installs plain text logging at `info`, for a process that cannot read its
/// settings or start its runtime and only needs to report why.
pub fn install_fallback() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info"))
        .try_init();
}

fn build_provider(
    service: &'static str,
    trace: &TraceSettings,
) -> Result<SdkTracerProvider, TelemetryError> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(TelemetryError::NoRuntime);
    }
    trace_filter(&trace.filter)?;
    export::provider(service, trace)
}

fn subscriber(
    settings: &Settings,
    service: &'static str,
    provider: Option<&SdkTracerProvider>,
) -> Result<(), TelemetryError> {
    // An unparsable `RUST_LOG` falls back to `info`, as it always has.
    let log_filter = settings
        .log_filter
        .as_deref()
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let logs = match settings.log_format {
        LogFormat::Text => tracing_subscriber::fmt::layer().boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed(),
    }
    .with_filter(log_filter)
    .boxed();
    let mut layers = vec![logs];
    if let (Some(provider), Some(trace)) = (provider, &settings.trace) {
        layers.push(
            exporting_layer(provider.tracer(service))
                .with_filter(trace_filter(&trace.filter)?)
                .boxed(),
        );
    }
    Registry::default()
        .with(layers)
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInstalled)
}

/// The tracing-to-OpenTelemetry layer. Entering a span does not start it:
/// a span gets its trace ID and sampling decision when it first needs a
/// context or ends, so [`RemoteParent::adopt`] can still give it a parent
/// after the caller is authenticated.
fn exporting_layer<S>(
    tracer: opentelemetry_sdk::trace::SdkTracer,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_context_activation(false)
}

/// Bergsmith's own targets at the levels in `directives`. Dependency targets
/// are excluded whatever the directives say, which also keeps the
/// exporter's own requests out of the traces it exports.
fn trace_filter(
    directives: &str,
) -> Result<impl tracing_subscriber::layer::Filter<Registry> + use<>, TelemetryError> {
    let levels = EnvFilter::try_new(directives).map_err(|error| TelemetryError::Filter {
        name: "BERGSMITH_TRACE_FILTER",
        reason: error.to_string(),
    })?;
    Ok(filter_fn(|metadata| is_bergsmith(metadata.target())).and(levels))
}

fn is_bergsmith(target: &str) -> bool {
    target
        .split("::")
        .next()
        .is_some_and(|root| root == "bergsmith" || root.starts_with("bergsmith_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bergsmith_targets_are_exported() {
        for target in [
            "bergsmith_coordinator::runtime::planning",
            "bergsmith_worker",
            "bergsmith",
        ] {
            assert!(is_bergsmith(target), "{target}");
        }
        for target in [
            "sqlx::query",
            "h2::codec",
            "tonic",
            "bergsmithy",
            "opentelemetry_sdk",
        ] {
            assert!(!is_bergsmith(target), "{target}");
        }
    }

    #[test]
    fn an_invalid_trace_filter_names_its_variable() {
        assert!(matches!(
            trace_filter("bergsmith=loud"),
            Err(TelemetryError::Filter {
                name: "BERGSMITH_TRACE_FILTER",
                ..
            })
        ));
    }
}
