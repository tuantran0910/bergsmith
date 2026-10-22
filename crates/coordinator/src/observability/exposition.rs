//! Prometheus text exposition format (version 0.0.4).
//!
//! Families are written in the order they are added and samples in the order
//! given, so output is deterministic for ordered inputs.

use std::fmt::Write as _;

/// The content type of the text format.
pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A metric family's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// Builds one exposition document.
#[derive(Debug, Default)]
pub(crate) struct Exposition {
    text: String,
}

impl Exposition {
    /// Starts a family: its `HELP` and `TYPE` lines.
    pub(crate) fn family(&mut self, name: &str, kind: Kind, help: &str) {
        let help = help.replace('\\', "\\\\").replace('\n', "\\n");
        let _ = writeln!(self.text, "# HELP {name} {help}");
        let _ = writeln!(self.text, "# TYPE {name} {}", kind.as_str());
    }

    /// Writes one sample of the current family.
    pub(crate) fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.text.push_str(name);
        if !labels.is_empty() {
            self.text.push('{');
            for (index, (label, value)) in labels.iter().enumerate() {
                if index > 0 {
                    self.text.push(',');
                }
                let _ = write!(self.text, "{label}=\"{}\"", escape(value));
            }
            self.text.push('}');
        }
        let _ = writeln!(self.text, " {}", number(value));
    }

    /// The finished document.
    pub(crate) fn finish(self) -> String {
        self.text
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "+Inf".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Inf".to_owned()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_and_samples_follow_the_text_format() {
        let mut out = Exposition::default();
        out.family("bergsmith_jobs", Kind::Gauge, "Jobs\nby state.");
        out.sample("bergsmith_jobs", &[("state", "Queued")], 3.0);
        out.sample("bergsmith_jobs", &[("a", "x\"y\\z\n"), ("b", "")], 0.5);
        out.family("bergsmith_up", Kind::Counter, "Up.");
        out.sample("bergsmith_up", &[], f64::INFINITY);
        out.family("bergsmith_h", Kind::Histogram, "H.");
        out.sample("bergsmith_h", &[], f64::NAN);
        out.sample("bergsmith_h", &[], f64::NEG_INFINITY);

        assert_eq!(
            out.finish(),
            "# HELP bergsmith_jobs Jobs\\nby state.\n\
             # TYPE bergsmith_jobs gauge\n\
             bergsmith_jobs{state=\"Queued\"} 3\n\
             bergsmith_jobs{a=\"x\\\"y\\\\z\\n\",b=\"\"} 0.5\n\
             # HELP bergsmith_up Up.\n\
             # TYPE bergsmith_up counter\n\
             bergsmith_up +Inf\n\
             # HELP bergsmith_h H.\n\
             # TYPE bergsmith_h histogram\n\
             bergsmith_h NaN\n\
             bergsmith_h -Inf\n"
        );
    }
}
