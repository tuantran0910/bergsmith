//! Conversions between wire types and `bergsmith-common` types.
//!
//! Wire values are untrusted: identifiers are parsed, and unspecified or
//! unknown enum values are rejected instead of guessed.

use std::str::FromStr;

use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::IdParseError;
use bergsmith_common::job::JobState;
use bergsmith_common::task::AttemptState;

use crate::common as wire;

/// A wire value that does not convert to its domain type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("field {field}: {problem}")]
pub struct ConversionError {
    /// The message field that failed to convert.
    pub field: &'static str,
    /// What was wrong with it.
    pub problem: ConversionProblem,
}

/// Why a wire value failed to convert.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionProblem {
    /// A required field was absent.
    #[error("missing")]
    Missing,
    /// An enum field held its `UNSPECIFIED` zero value.
    #[error("unspecified")]
    Unspecified,
    /// An enum field held a value this build does not know.
    #[error("unknown enum value {0}")]
    UnknownEnumValue(i32),
    /// A string did not parse as the expected identifier.
    #[error("invalid identifier: {0}")]
    InvalidId(String),
}

impl ConversionError {
    /// Creates an error for `field`.
    #[must_use]
    pub const fn new(field: &'static str, problem: ConversionProblem) -> Self {
        Self { field, problem }
    }
}

/// Parses a wire identifier into its typed ID.
///
/// # Errors
///
/// [`ConversionProblem::Missing`] for an empty string and
/// [`ConversionProblem::InvalidId`] for anything that is not a UUID.
pub fn parse_id<T>(field: &'static str, value: &str) -> Result<T, ConversionError>
where
    T: FromStr<Err = IdParseError>,
{
    if value.is_empty() {
        return Err(ConversionError::new(field, ConversionProblem::Missing));
    }
    value.parse().map_err(|error: IdParseError| {
        ConversionError::new(field, ConversionProblem::InvalidId(error.to_string()))
    })
}

/// Returns a required message field, or a [`ConversionProblem::Missing`] error.
///
/// # Errors
///
/// [`ConversionProblem::Missing`] when `value` is `None`.
pub fn required<T>(field: &'static str, value: Option<T>) -> Result<T, ConversionError> {
    value.ok_or(ConversionError::new(field, ConversionProblem::Missing))
}

/// Defines lossless conversions between a domain enum and its wire enum.
macro_rules! enum_conversions {
    ($domain:ty, $wire:ty, $to_domain:ident, $to_wire:ident, [$($d:ident <=> $w:ident),+ $(,)?]) => {
        impl From<$domain> for $wire {
            fn from(value: $domain) -> Self {
                match value {
                    $(<$domain>::$d => <$wire>::$w,)+
                }
            }
        }

        /// Converts the domain enum into its raw wire value.
        #[must_use]
        pub fn $to_wire(value: $domain) -> i32 {
            <$wire>::from(value).into()
        }

        /// Converts a raw wire value for `field` into the domain enum.
        ///
        /// # Errors
        ///
        /// [`ConversionProblem::Unspecified`] for the zero value and
        /// [`ConversionProblem::UnknownEnumValue`] for values this build
        /// does not know.
        pub fn $to_domain(field: &'static str, value: i32) -> Result<$domain, ConversionError> {
            match <$wire>::try_from(value) {
                $(Ok(<$wire>::$w) => Ok(<$domain>::$d),)+
                Ok(_) => Err(ConversionError::new(field, ConversionProblem::Unspecified)),
                Err(_) => Err(ConversionError::new(field, ConversionProblem::UnknownEnumValue(value))),
            }
        }
    };
}

enum_conversions!(JobState, wire::JobState, job_state, job_state_to_wire, [
    Planning <=> Planning,
    Queued <=> Queued,
    Running <=> Running,
    ReadyToCommit <=> ReadyToCommit,
    Committing <=> Committing,
    CommitOutcomeUnknown <=> CommitOutcomeUnknown,
    Deleting <=> Deleting,
    Cancelling <=> Cancelling,
    Succeeded <=> Succeeded,
    NoOp <=> NoOp,
    Cancelled <=> Cancelled,
    ReplanRequired <=> ReplanRequired,
    Failed <=> Failed,
]);

enum_conversions!(AttemptState, wire::AttemptState, attempt_state, attempt_state_to_wire, [
    Granted <=> Granted,
    Running <=> Running,
    Accepted <=> Accepted,
    Failed <=> Failed,
    Expired <=> Expired,
    Cancelled <=> Cancelled,
]);

enum_conversions!(
    FailureDisposition,
    wire::FailureDisposition,
    failure_disposition,
    failure_disposition_to_wire,
    [
    RetrySameRequest <=> RetrySameRequest,
    RetryTask <=> RetryTask,
    ReplanRequired <=> ReplanRequired,
    PermanentFailure <=> PermanentFailure,
    ResolvePublication <=> ResolvePublication,
]);

#[cfg(test)]
mod tests {
    use bergsmith_common::ids::{AttemptId, JobId};

    use super::*;

    #[test]
    fn every_job_state_round_trips() {
        for state in JobState::ALL {
            assert_eq!(job_state("job_state", job_state_to_wire(state)), Ok(state));
        }
    }

    #[test]
    fn every_attempt_state_round_trips() {
        for state in AttemptState::ALL {
            assert_eq!(
                attempt_state("attempt_state", attempt_state_to_wire(state)),
                Ok(state)
            );
        }
    }

    #[test]
    fn every_failure_disposition_round_trips() {
        for disposition in FailureDisposition::ALL {
            assert_eq!(
                failure_disposition("disposition", failure_disposition_to_wire(disposition)),
                Ok(disposition)
            );
        }
    }

    /// Counts the known wire values after zero, checking each converts.
    fn convertible_wire_values<T>(
        known: impl Fn(i32) -> bool,
        convert: impl Fn(i32) -> Result<T, ConversionError>,
    ) -> usize {
        (1..)
            .take_while(|value| known(*value))
            .inspect(|value| assert!(convert(*value).is_ok(), "wire value {value}"))
            .count()
    }

    #[test]
    fn every_wire_value_has_a_domain_value() {
        let jobs = convertible_wire_values(
            |v| wire::JobState::try_from(v).is_ok(),
            |v| job_state("job_state", v),
        );
        let attempts = convertible_wire_values(
            |v| wire::AttemptState::try_from(v).is_ok(),
            |v| attempt_state("attempt_state", v),
        );
        let dispositions = convertible_wire_values(
            |v| wire::FailureDisposition::try_from(v).is_ok(),
            |v| failure_disposition("disposition", v),
        );

        assert_eq!(jobs, JobState::ALL.len());
        assert_eq!(attempts, AttemptState::ALL.len());
        assert_eq!(dispositions, FailureDisposition::ALL.len());
    }

    #[test]
    fn unspecified_and_unknown_enum_values_are_rejected() {
        assert_eq!(
            job_state("job_state", 0).unwrap_err().problem,
            ConversionProblem::Unspecified
        );
        assert_eq!(
            attempt_state("attempt_state", 99).unwrap_err(),
            ConversionError::new("attempt_state", ConversionProblem::UnknownEnumValue(99))
        );
    }

    #[test]
    fn identifiers_round_trip_and_bad_ones_name_the_field() {
        let id = JobId::generate();
        assert_eq!(parse_id::<JobId>("job_id", &id.to_string()), Ok(id));

        let missing = parse_id::<AttemptId>("attempt_id", "").unwrap_err();
        let invalid = parse_id::<AttemptId>("attempt_id", "nope").unwrap_err();

        assert_eq!(missing.problem, ConversionProblem::Missing);
        assert!(matches!(invalid.problem, ConversionProblem::InvalidId(_)));
        assert!(
            invalid
                .to_string()
                .starts_with("field attempt_id: invalid identifier")
        );
    }

    #[test]
    fn required_fields_report_their_name() {
        assert_eq!(required("grant", Some(1)), Ok(1));
        assert_eq!(
            required::<i32>("grant", None).unwrap_err().to_string(),
            "field grant: missing"
        );
    }
}
