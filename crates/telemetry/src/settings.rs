//! Telemetry settings read from the process environment (ADR 0068).
//!
//! Parsing is separate from installation so every rule is testable without
//! touching the real environment or a global subscriber.

use std::path::PathBuf;

use crate::TelemetryError;
use crate::sampler::SamplerChoice;

/// The log line format chosen by `BERGSMITH_LOG_FORMAT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogFormat {
    /// Human-readable lines, the default.
    Text,
    /// JSON lines with the current span and its parents.
    Json,
}

/// Everything [`crate::install`] needs, parsed and validated.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub(crate) log_format: LogFormat,
    /// `RUST_LOG`, or `None` for the `info` default.
    pub(crate) log_filter: Option<String>,
    /// `None` when trace export is off.
    pub(crate) trace: Option<TraceSettings>,
}

/// OTLP trace export settings. The exporter itself reads the remaining
/// standard variables (headers, timeout, compression, batch sizes).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TraceSettings {
    /// The resolved collector endpoint.
    pub(crate) endpoint: String,
    pub(crate) sampler: SamplerChoice,
    /// Level directives for Bergsmith's own targets.
    pub(crate) filter: String,
    /// `OTEL_SERVICE_NAME`, or `None` to keep the resource's or the default.
    pub(crate) service_name: Option<String>,
    /// A private CA bundle for an HTTPS collector.
    pub(crate) certificate: Option<PathBuf>,
}

/// The trace filter when `BERGSMITH_TRACE_FILTER` is unset.
pub(crate) const DEFAULT_TRACE_FILTER: &str = "info";

impl Settings {
    /// Reads the settings through `lookup`, which returns a variable's value
    /// or `None` when it is unset.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Setting`] names the first variable whose value is
    /// not supported.
    pub(crate) fn read(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, TelemetryError> {
        let set = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let log_format = match set("BERGSMITH_LOG_FORMAT").as_deref() {
            None | Some("text") => LogFormat::Text,
            Some("json") => LogFormat::Json,
            Some(_) => return Err(invalid("BERGSMITH_LOG_FORMAT", "must be text or json")),
        };
        Ok(Self {
            log_format,
            log_filter: set("RUST_LOG"),
            trace: trace_settings(&set)?,
        })
    }
}

fn trace_settings(
    set: &impl Fn(&str) -> Option<String>,
) -> Result<Option<TraceSettings>, TelemetryError> {
    let Some((endpoint_name, endpoint)) = [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ]
    .into_iter()
    .find_map(|name| set(name).map(|value| (name, value))) else {
        return Ok(None);
    };
    match set("OTEL_SDK_DISABLED").map(|value| value.trim().to_ascii_lowercase()) {
        None => {}
        Some(value) if value == "false" => {}
        Some(value) if value == "true" => return Ok(None),
        Some(_) => return Err(invalid("OTEL_SDK_DISABLED", "must be true or false")),
    }
    match set("OTEL_TRACES_EXPORTER").as_deref().map(str::trim) {
        None | Some("otlp") => {}
        Some("none") => return Ok(None),
        Some(_) => return Err(invalid("OTEL_TRACES_EXPORTER", "must be otlp or none")),
    }
    for name in [
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
    ] {
        if set(name).is_some_and(|value| value.trim() != "grpc") {
            return Err(invalid(name, "only grpc is supported"));
        }
    }
    for name in [
        "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
        "OTEL_EXPORTER_OTLP_COMPRESSION",
    ] {
        if set(name).is_some_and(|value| !matches!(value.trim(), "gzip" | "none")) {
            return Err(invalid(name, "must be gzip or none"));
        }
    }
    // Parse here so a malformed endpoint never reaches the exporter, whose
    // own error message repeats the endpoint and any credentials in it.
    let endpoint = endpoint.trim().to_owned();
    let valid = endpoint.parse::<http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri
                .authority()
                .is_some_and(|authority| !authority.host().is_empty())
    });
    if !valid {
        return Err(invalid(endpoint_name, "must be an http:// or https:// URL"));
    }
    Ok(Some(TraceSettings {
        endpoint,
        sampler: SamplerChoice::parse(
            set("OTEL_TRACES_SAMPLER").as_deref(),
            set("OTEL_TRACES_SAMPLER_ARG").as_deref(),
        )?,
        filter: set("BERGSMITH_TRACE_FILTER").unwrap_or_else(|| DEFAULT_TRACE_FILTER.to_owned()),
        service_name: set("OTEL_SERVICE_NAME").map(|name| name.trim().to_owned()),
        certificate: set("OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE")
            .or_else(|| set("OTEL_EXPORTER_OTLP_CERTIFICATE"))
            .map(PathBuf::from),
    }))
}

/// A setting error for `name`; the value is never repeated, because an
/// endpoint or header variable may carry a credential.
pub(crate) fn invalid(name: &'static str, reason: &'static str) -> TelemetryError {
    TelemetryError::Setting { name, reason }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::sampler::Root;

    fn read(pairs: &[(&str, &str)]) -> Result<Settings, TelemetryError> {
        let values: BTreeMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        Settings::read(|name| values.get(name).cloned())
    }

    fn setting_error(result: Result<Settings, TelemetryError>) -> &'static str {
        match result {
            Err(TelemetryError::Setting { name, .. }) => name,
            other => unreachable!("expected a setting error, got {other:?}"),
        }
    }

    #[test]
    fn export_is_off_without_an_endpoint() {
        let settings = read(&[("OTEL_TRACES_SAMPLER", "always_on")]).unwrap();
        assert_eq!(settings.log_format, LogFormat::Text);
        assert_eq!(settings.trace, None);
    }

    #[test]
    fn an_empty_endpoint_counts_as_unset() {
        assert_eq!(
            read(&[("OTEL_EXPORTER_OTLP_ENDPOINT", " ")]).unwrap().trace,
            None
        );
    }

    #[test]
    fn the_traces_endpoint_wins_over_the_shared_one() {
        let settings = read(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://shared:4317"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://traces:4317"),
        ])
        .unwrap();
        let trace = settings.trace.unwrap();
        assert_eq!(trace.endpoint, "https://traces:4317");
        assert_eq!(
            trace.sampler,
            SamplerChoice {
                root: Root::AlwaysOn,
                parent_based: true
            }
        );
        assert_eq!(trace.filter, DEFAULT_TRACE_FILTER);
        assert_eq!(trace.service_name, None);
        assert_eq!(trace.certificate, None);
    }

    #[test]
    fn disabling_switches_turn_export_off() {
        for (name, value) in [
            ("OTEL_SDK_DISABLED", "true"),
            ("OTEL_SDK_DISABLED", "TRUE"),
            ("OTEL_TRACES_EXPORTER", "none"),
        ] {
            let settings = read(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317"),
                (name, value),
            ]);
            assert_eq!(settings.unwrap().trace, None, "{name}={value}");
        }
        let enabled = read(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317"),
            ("OTEL_SDK_DISABLED", "false"),
            ("OTEL_TRACES_EXPORTER", "otlp"),
        ]);
        assert!(enabled.unwrap().trace.is_some());
    }

    #[test]
    fn unsupported_values_name_their_variable() {
        let endpoint = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317");
        for (pair, expected) in [
            (("BERGSMITH_LOG_FORMAT", "yaml"), "BERGSMITH_LOG_FORMAT"),
            (("OTEL_SDK_DISABLED", "yes"), "OTEL_SDK_DISABLED"),
            (("OTEL_TRACES_EXPORTER", "zipkin"), "OTEL_TRACES_EXPORTER"),
            (
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
                "OTEL_EXPORTER_OTLP_PROTOCOL",
            ),
            (
                ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json"),
                "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            ),
            (
                ("OTEL_TRACES_SAMPLER", "jaeger_remote"),
                "OTEL_TRACES_SAMPLER",
            ),
        ] {
            assert_eq!(setting_error(read(&[endpoint, pair])), expected, "{pair:?}");
        }
        for (name, value) in [
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "collector:4317"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://collector:4317"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://"),
            // The name is the variable that was used, not the shared one.
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://user:pa ss@collector:4317",
            ),
        ] {
            assert_eq!(setting_error(read(&[(name, value)])), name, "{value}");
        }
        for name in [
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
        ] {
            assert_eq!(setting_error(read(&[endpoint, (name, "zstd")])), name);
            assert!(read(&[endpoint, (name, "gzip")]).is_ok());
        }
    }

    #[test]
    fn the_error_never_repeats_the_value() {
        for endpoint in [
            "user:secret-password@collector",
            // A valid prefix but an unparsable URI must not reach the
            // exporter, whose own error repeats the endpoint.
            "https://user:secret password@collector:4317",
        ] {
            let error = read(&[("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)]).unwrap_err();
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[test]
    fn json_logs_and_trace_options_are_read() {
        let settings = read(&[
            ("BERGSMITH_LOG_FORMAT", "json"),
            ("RUST_LOG", "debug"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://c:4317"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_TRACES_SAMPLER", "parentbased_traceidratio"),
            ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
            ("BERGSMITH_TRACE_FILTER", "debug"),
            ("OTEL_SERVICE_NAME", "coordinator-a"),
            ("OTEL_EXPORTER_OTLP_CERTIFICATE", "/etc/ca.pem"),
        ])
        .unwrap();
        assert_eq!(settings.log_format, LogFormat::Json);
        assert_eq!(settings.log_filter.as_deref(), Some("debug"));
        let trace = settings.trace.unwrap();
        assert_eq!(
            trace.sampler,
            SamplerChoice {
                root: Root::Ratio(0.25),
                parent_based: true
            }
        );
        assert_eq!(trace.filter, "debug");
        assert_eq!(trace.service_name.as_deref(), Some("coordinator-a"));
        assert_eq!(trace.certificate, Some(PathBuf::from("/etc/ca.pem")));
    }
}
