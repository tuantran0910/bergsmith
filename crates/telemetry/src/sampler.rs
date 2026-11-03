//! The trace sampler (ADR 0068).
//!
//! The standard parent-based sampler samples every span whose remote parent
//! says "sampled". A caller that has not authenticated yet could then raise
//! the sample rate just by sending `traceparent`. Here a remote sampled
//! parent is still subject to the root sampler. Trace-ID ratio sampling is a
//! pure function of the trace ID, so a worker and the coordinator configured
//! with the same ratio still agree on every trace.

use opentelemetry::trace::{Link, SpanKind, TraceContextExt, TraceId};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{Sampler, SamplingDecision, SamplingResult, ShouldSample};

use crate::TelemetryError;
use crate::settings::invalid;

/// How a span without a sampled local parent is sampled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Root {
    /// Sample every trace.
    AlwaysOn,
    /// Sample no trace.
    AlwaysOff,
    /// Sample this fraction of trace IDs, in `0.0..=1.0`.
    Ratio(f64),
}

/// The sampler selected by `OTEL_TRACES_SAMPLER` and its argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SamplerChoice {
    pub(crate) root: Root,
    /// Whether a parent's decision is followed (the `parentbased_` forms).
    pub(crate) parent_based: bool,
}

impl SamplerChoice {
    /// Parses `OTEL_TRACES_SAMPLER` and `OTEL_TRACES_SAMPLER_ARG`. The
    /// default is `parentbased_always_on`; a ratio sampler without an
    /// argument samples everything, as the specification requires.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Setting`] for another sampler, or for an argument
    /// that is not a number from 0 to 1.
    pub(crate) fn parse(
        name: Option<&str>,
        argument: Option<&str>,
    ) -> Result<Self, TelemetryError> {
        let name = name.map_or("parentbased_always_on", str::trim);
        let (parent_based, root) = match name.strip_prefix("parentbased_") {
            Some(root) => (true, root),
            None => (false, name),
        };
        let root = match root {
            "always_on" => Root::AlwaysOn,
            "always_off" => Root::AlwaysOff,
            "traceidratio" => Root::Ratio(match argument {
                None => 1.0,
                Some(argument) => argument
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|ratio| (0.0..=1.0).contains(ratio))
                    .ok_or_else(|| {
                        invalid("OTEL_TRACES_SAMPLER_ARG", "must be a number from 0 to 1")
                    })?,
            }),
            _ => {
                return Err(invalid(
                    "OTEL_TRACES_SAMPLER",
                    "must be always_on, always_off, traceidratio, or a parentbased_ form",
                ));
            }
        };
        Ok(Self { root, parent_based })
    }
}

/// The installed sampler; see the module documentation.
#[derive(Clone, Debug)]
pub(crate) struct BoundedSampler {
    root: Sampler,
    parent_based: bool,
}

impl From<SamplerChoice> for BoundedSampler {
    fn from(choice: SamplerChoice) -> Self {
        Self {
            root: match choice.root {
                Root::AlwaysOn => Sampler::AlwaysOn,
                Root::AlwaysOff => Sampler::AlwaysOff,
                Root::Ratio(ratio) => Sampler::TraceIdRatioBased(ratio),
            },
            parent_based: choice.parent_based,
        }
    }
}

impl ShouldSample for BoundedSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        let parent = parent_context
            .filter(|context| context.has_active_span())
            .map(|context| context.span().span_context().clone())
            .filter(opentelemetry::trace::SpanContext::is_valid);
        let delegate = match parent {
            Some(parent) if self.parent_based && !parent.is_sampled() => &Sampler::AlwaysOff,
            Some(parent) if self.parent_based && !parent.is_remote() => &Sampler::AlwaysOn,
            // A remote sampled parent, no parent, or a sampler that ignores
            // parents: the root decides.
            _ => &self.root,
        };
        let result =
            delegate.should_sample(parent_context, trace_id, name, span_kind, attributes, links);
        debug_assert!(matches!(
            result.decision,
            SamplingDecision::Drop | SamplingDecision::RecordAndSample
        ));
        result
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceState};

    use super::*;

    fn choice(name: &str, argument: Option<&str>) -> SamplerChoice {
        SamplerChoice::parse(Some(name), argument).unwrap()
    }

    #[test]
    fn every_supported_sampler_parses() {
        let cases = [
            ("always_on", None, Root::AlwaysOn, false),
            ("always_off", None, Root::AlwaysOff, false),
            ("traceidratio", Some("0.5"), Root::Ratio(0.5), false),
            ("traceidratio", None, Root::Ratio(1.0), false),
            ("parentbased_always_on", None, Root::AlwaysOn, true),
            ("parentbased_always_off", None, Root::AlwaysOff, true),
            (
                "parentbased_traceidratio",
                Some(" 0 "),
                Root::Ratio(0.0),
                true,
            ),
        ];
        for (name, argument, root, parent_based) in cases {
            assert_eq!(
                choice(name, argument),
                SamplerChoice { root, parent_based },
                "{name}"
            );
        }
        assert_eq!(
            SamplerChoice::parse(None, None).unwrap(),
            SamplerChoice {
                root: Root::AlwaysOn,
                parent_based: true
            }
        );
    }

    #[test]
    fn invalid_samplers_and_ratios_are_rejected() {
        for (name, argument) in [
            ("jaeger_remote", None),
            ("parentbased_jaeger_remote", None),
            ("traceidratio", Some("1.5")),
            ("traceidratio", Some("-0.1")),
            ("traceidratio", Some("half")),
            ("traceidratio", Some("NaN")),
        ] {
            assert!(
                SamplerChoice::parse(Some(name), argument).is_err(),
                "{name} {argument:?}"
            );
        }
    }

    fn parent(sampled: bool, remote: bool) -> Context {
        let flags = if sampled {
            TraceFlags::SAMPLED
        } else {
            TraceFlags::default()
        };
        let span = SpanContext::new(
            TraceId::from(7),
            SpanId::from(9),
            flags,
            remote,
            TraceState::default(),
        );
        Context::new().with_remote_span_context(span)
    }

    fn sampled(sampler: &BoundedSampler, parent: Option<&Context>, trace_id: u128) -> bool {
        let result = sampler.should_sample(
            parent,
            TraceId::from(trace_id),
            "span",
            &SpanKind::Internal,
            &[],
            &[],
        );
        result.decision == SamplingDecision::RecordAndSample
    }

    #[test]
    fn a_remote_sampled_parent_cannot_raise_the_ratio() {
        let sampler = BoundedSampler::from(choice("parentbased_traceidratio", Some("0")));
        assert!(!sampled(&sampler, Some(&parent(true, true)), 7));
        assert!(!sampled(&sampler, None, 7));
    }

    #[test]
    fn a_local_sampled_parent_keeps_its_children() {
        // A local parent was itself admitted by the root sampler; dropping
        // its children would leave holes in the trace.
        let sampler = BoundedSampler::from(choice("parentbased_traceidratio", Some("0")));
        let local = parent(true, false);
        assert!(sampled(&sampler, Some(&local), 7));
    }

    #[test]
    fn an_unsampled_parent_drops_its_children() {
        let sampler = BoundedSampler::from(choice("parentbased_always_on", None));
        assert!(!sampled(&sampler, Some(&parent(false, true)), 7));
        assert!(!sampled(&sampler, Some(&parent(false, false)), 7));
        assert!(sampled(&sampler, Some(&parent(true, true)), 7));
        assert!(sampled(&sampler, None, 7));
    }

    #[test]
    fn a_sampler_without_parentbased_ignores_parents() {
        let on = BoundedSampler::from(choice("always_on", None));
        assert!(sampled(&on, Some(&parent(false, true)), 7));
        let off = BoundedSampler::from(choice("always_off", None));
        assert!(!sampled(&off, Some(&parent(true, false)), 7));
    }

    #[test]
    fn ratio_decisions_depend_only_on_the_trace_id() {
        // Worker and coordinator must agree on a shared trace.
        let worker = BoundedSampler::from(choice("parentbased_traceidratio", Some("0.5")));
        let coordinator = worker.clone();
        let mut kept = 0;
        for trace_id in 1..=2_000_u128 {
            let id = trace_id << 64 | trace_id.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let decision = sampled(&worker, None, id);
            assert_eq!(
                decision,
                sampled(&coordinator, Some(&parent(true, true)), id)
            );
            kept += u32::from(decision);
        }
        assert!((800..1_200).contains(&kept), "kept {kept} of 2000");
    }
}
