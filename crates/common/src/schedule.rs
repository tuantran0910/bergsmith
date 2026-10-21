//! Five-field cron expressions evaluated in IANA time zones (BS-019.T1).
//!
//! A schedule names when maintenance may start, not what it does. This module
//! parses the timing contract and resolves the next start instant, including
//! the daylight-saving rules required by the plan (§5 "Scheduling semantics"):
//!
//! - A local time that does not exist during a spring-forward gap is skipped.
//! - A local time that happens twice during a fall-back overlap runs once, at
//!   the earlier instant. The earlier instant is the occurrence identity, so an
//!   instant between the two readings does not produce a second run.
//!
//! The expression syntax is the classic five-field form, minute first:
//! `minute hour day-of-month month day-of-week`. Each field accepts `*`, a
//! number, a name where one exists (months and weekdays), a range, a step, and
//! comma-separated lists. Day-of-week accepts `0`–`7`, where both `0` and `7`
//! mean Sunday. When both day-of-month and day-of-week are restricted, a date
//! matches if either field matches (the Vixie cron rule). The design is
//! recorded in ADR 0038.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

/// How far ahead [`CronSchedule::next_occurrence`] searches before reporting
/// that an expression never matches.
///
/// Nine years covers the longest gap between leap days: 2100 is not a leap
/// year, so the next 29 February after 2096 is 2104. An expression such as
/// `0 0 30 2 *` names a date that never exists, so the search must terminate.
const MAX_SEARCH_DAYS: i64 = 366 * 9;

/// Why a cron expression or time zone was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    /// The expression did not contain exactly five fields.
    #[error("a cron expression needs five fields, not {found}")]
    FieldCount {
        /// How many whitespace-separated fields were found.
        found: usize,
    },
    /// One field was empty, as in `0,,5 * * * *`.
    #[error("the cron {field} field is empty")]
    EmptyField {
        /// The field that was empty.
        field: &'static str,
    },
    /// A value was outside the field's range or used unknown syntax.
    #[error("the cron {field} field has an invalid value {value:?}")]
    InvalidField {
        /// The field that failed.
        field: &'static str,
        /// The rejected text.
        value: String,
    },
    /// A range ended before it started.
    #[error("the cron {field} field has a reversed range {value:?}")]
    ReversedRange {
        /// The field that failed.
        field: &'static str,
        /// The rejected text.
        value: String,
    },
    /// A step was zero.
    #[error("the cron {field} field step must be positive")]
    ZeroStep {
        /// The field that failed.
        field: &'static str,
    },
    /// The time zone was not an IANA name.
    #[error("time zone {value:?} is not an IANA name")]
    UnknownTimeZone {
        /// The rejected text.
        value: String,
    },
}

/// One field's accepted range and names.
struct FieldSpec {
    /// Field name used in errors.
    name: &'static str,
    /// Smallest accepted numeric value.
    min: u8,
    /// Largest accepted numeric value.
    max: u8,
    /// Three-letter names, when the field accepts them.
    names: &'static [(&'static str, u8)],
    /// Whether `7` is an alias for `0` (day-of-week only).
    seven_is_zero: bool,
}

const MONTHS: &[(&str, u8)] = &[
    ("JAN", 1),
    ("FEB", 2),
    ("MAR", 3),
    ("APR", 4),
    ("MAY", 5),
    ("JUN", 6),
    ("JUL", 7),
    ("AUG", 8),
    ("SEP", 9),
    ("OCT", 10),
    ("NOV", 11),
    ("DEC", 12),
];

const WEEKDAYS: &[(&str, u8)] = &[
    ("SUN", 0),
    ("MON", 1),
    ("TUE", 2),
    ("WED", 3),
    ("THU", 4),
    ("FRI", 5),
    ("SAT", 6),
];

const MINUTE: FieldSpec = FieldSpec {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
    seven_is_zero: false,
};
const HOUR: FieldSpec = FieldSpec {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
    seven_is_zero: false,
};
const DAY_OF_MONTH: FieldSpec = FieldSpec {
    name: "day-of-month",
    min: 1,
    max: 31,
    names: &[],
    seven_is_zero: false,
};
const MONTH: FieldSpec = FieldSpec {
    name: "month",
    min: 1,
    max: 12,
    names: MONTHS,
    seven_is_zero: false,
};
const DAY_OF_WEEK: FieldSpec = FieldSpec {
    name: "day-of-week",
    min: 0,
    max: 7,
    names: WEEKDAYS,
    seven_is_zero: true,
};

impl FieldSpec {
    /// Parses one field, rejecting everything outside the accepted syntax.
    fn parse(&self, text: &str) -> Result<BTreeSet<u8>, ScheduleError> {
        if text.is_empty() {
            return Err(ScheduleError::EmptyField { field: self.name });
        }
        let mut values = BTreeSet::new();
        for item in text.split(',') {
            self.parse_item(item, &mut values)?;
        }
        if self.seven_is_zero && values.remove(&7) {
            values.insert(0);
        }
        Ok(values)
    }

    /// Parses one list item: a value, range, or step over either.
    fn parse_item(&self, item: &str, values: &mut BTreeSet<u8>) -> Result<(), ScheduleError> {
        if item.is_empty() {
            return Err(ScheduleError::EmptyField { field: self.name });
        }
        let (base, step) = match item.split_once('/') {
            None => (item, 1),
            Some((base, step)) => {
                if step.contains('/') {
                    return Err(self.invalid(item));
                }
                let step = step.parse::<u8>().map_err(|_| self.invalid(step))?;
                if step == 0 {
                    return Err(ScheduleError::ZeroStep { field: self.name });
                }
                (base, step)
            }
        };
        let has_step = step != 1 || item.contains('/');
        let (start, end) = if base == "*" {
            (self.min, self.max)
        } else if let Some((start, end)) = base.split_once('-') {
            let start = self.value(start)?;
            let end = self.value(end)?;
            if start > end {
                return Err(ScheduleError::ReversedRange {
                    field: self.name,
                    value: item.to_owned(),
                });
            }
            (start, end)
        } else {
            let start = self.value(base)?;
            // `n/step` steps from `n` to the end of the field, as in Vixie cron.
            (start, if has_step { self.max } else { start })
        };
        let mut value = start;
        while value <= end {
            values.insert(value);
            match value.checked_add(step) {
                Some(next) if next > value => value = next,
                _ => break,
            }
        }
        Ok(())
    }

    /// Parses a number or a three-letter name in this field.
    fn value(&self, text: &str) -> Result<u8, ScheduleError> {
        let text = text.trim();
        if let Ok(value) = text.parse::<u8>() {
            return if (self.min..=self.max).contains(&value) {
                Ok(value)
            } else {
                Err(self.invalid(text))
            };
        }
        self.names
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(text))
            .map(|(_, value)| *value)
            .ok_or_else(|| self.invalid(text))
    }

    fn invalid(&self, value: &str) -> ScheduleError {
        ScheduleError::InvalidField {
            field: self.name,
            value: value.to_owned(),
        }
    }

    /// Whether a parsed set covers the whole field, which makes it unrestricted.
    fn is_unrestricted(&self, values: &BTreeSet<u8>) -> bool {
        let (min, max) = if self.seven_is_zero {
            (0, 6)
        } else {
            (self.min, self.max)
        };
        (min..=max).all(|value| values.contains(&value))
    }
}

/// A parsed five-field cron expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpression {
    /// The canonical form: one space between fields.
    text: String,
    minutes: BTreeSet<u8>,
    hours: BTreeSet<u8>,
    days_of_month: BTreeSet<u8>,
    months: BTreeSet<u8>,
    days_of_week: BTreeSet<u8>,
    /// Whether `day-of-month` restricts the date at all.
    day_of_month_restricted: bool,
    /// Whether `day-of-week` restricts the date at all.
    day_of_week_restricted: bool,
}

impl CronExpression {
    /// Parses a five-field expression.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::FieldCount`] when there are not exactly five fields,
    /// and the field variants of [`ScheduleError`] for malformed syntax.
    pub fn parse(expression: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ScheduleError::FieldCount {
                found: fields.len(),
            });
        }
        let minutes = MINUTE.parse(fields[0])?;
        let hours = HOUR.parse(fields[1])?;
        let days_of_month = DAY_OF_MONTH.parse(fields[2])?;
        let months = MONTH.parse(fields[3])?;
        let days_of_week = DAY_OF_WEEK.parse(fields[4])?;
        Ok(Self {
            text: fields.join(" "),
            day_of_month_restricted: !DAY_OF_MONTH.is_unrestricted(&days_of_month),
            day_of_week_restricted: !DAY_OF_WEEK.is_unrestricted(&days_of_week),
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
        })
    }

    /// The canonical expression text, as persisted.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The minute values that match, in ascending order.
    pub fn minutes(&self) -> impl Iterator<Item = u8> + '_ {
        self.minutes.iter().copied()
    }

    /// The hour values that match, in ascending order.
    pub fn hours(&self) -> impl Iterator<Item = u8> + '_ {
        self.hours.iter().copied()
    }

    /// Whether a date matches the day-of-month, month, and day-of-week fields.
    #[must_use]
    pub fn matches_date(&self, date: NaiveDate) -> bool {
        let month_matches =
            u8::try_from(date.month()).is_ok_and(|month| self.months.contains(&month));
        if !month_matches {
            return false;
        }
        let day_of_month =
            u8::try_from(date.day()).is_ok_and(|day| self.days_of_month.contains(&day));
        let day_of_week = u8::try_from(date.weekday().num_days_from_sunday())
            .is_ok_and(|weekday| self.days_of_week.contains(&weekday));
        match (self.day_of_month_restricted, self.day_of_week_restricted) {
            (false, false) => true,
            (true, false) => day_of_month,
            (false, true) => day_of_week,
            // Vixie cron: a date matches when either restricted field matches.
            (true, true) => day_of_month || day_of_week,
        }
    }
}

impl fmt::Display for CronExpression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for CronExpression {
    type Err = ScheduleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// An IANA time zone name, such as `Europe/Berlin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeZoneId {
    /// The canonical name reported by the time-zone database.
    name: String,
    /// The resolved zone.
    zone: Tz,
}

impl TimeZoneId {
    /// Parses and canonicalizes an IANA time-zone name.
    ///
    /// A name must be `UTC` or contain a `/`, which every regional IANA name
    /// does. Abbreviations such as `EST`, `MST`, or `HST` are rejected because
    /// they name a fixed offset and do not describe daylight-saving
    /// transitions; `EST` would otherwise silently pin a summer schedule to
    /// 14:00 UTC instead of New York's 13:00 UTC.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::UnknownTimeZone`] for an abbreviation or when the
    /// database does not know the name.
    pub fn parse(name: &str) -> Result<Self, ScheduleError> {
        let name = name.trim();
        if name != "UTC" && !name.contains('/') {
            return Err(ScheduleError::UnknownTimeZone {
                value: name.to_owned(),
            });
        }
        let zone = Tz::from_str(name).map_err(|_| ScheduleError::UnknownTimeZone {
            value: name.to_owned(),
        })?;
        Ok(Self {
            name: zone.name().to_owned(),
            zone,
        })
    }

    /// The canonical IANA name, as persisted.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// The resolved zone.
    #[must_use]
    pub const fn zone(&self) -> Tz {
        self.zone
    }
}

impl fmt::Display for TimeZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

impl FromStr for TimeZoneId {
    type Err = ScheduleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// When a schedule may start, and in which local time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    /// What matches.
    pub expression: CronExpression,
    /// The local time zone occurrences are expressed in.
    pub time_zone: TimeZoneId,
}

impl CronSchedule {
    /// Builds a schedule from its persisted parts.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`CronExpression::parse`] and
    /// [`TimeZoneId::parse`].
    pub fn parse(expression: &str, time_zone: &str) -> Result<Self, ScheduleError> {
        Ok(Self {
            expression: CronExpression::parse(expression)?,
            time_zone: TimeZoneId::parse(time_zone)?,
        })
    }

    /// The first start instant strictly after `after`, in UTC.
    ///
    /// A local time skipped by a spring-forward transition is not returned.
    /// When a fall-back transition repeats a local time, only the earlier
    /// instant is an occurrence.
    ///
    /// Returns `None` when the expression names no date at all, such as
    /// `0 0 30 2 *`.
    #[must_use]
    pub fn next_occurrence(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let zone = self.time_zone.zone();
        let start = after.with_timezone(&zone).date_naive();
        for offset in 0..=MAX_SEARCH_DAYS {
            let date = start.checked_add_signed(Duration::days(offset))?;
            if !self.expression.matches_date(date) {
                continue;
            }
            let mut next: Option<DateTime<Utc>> = None;
            for hour in self.expression.hours() {
                for minute in self.expression.minutes() {
                    let Some(local) = date.and_hms_opt(u32::from(hour), u32::from(minute), 0)
                    else {
                        continue;
                    };
                    let Some(instant) = resolve_local(zone, &local) else {
                        continue;
                    };
                    if instant > after && next.is_none_or(|current| instant < current) {
                        next = Some(instant);
                    }
                }
            }
            if next.is_some() {
                return next;
            }
        }
        None
    }
}

/// Resolves a local time to one instant, applying the plan's DST rules.
fn resolve_local(zone: Tz, local: &NaiveDateTime) -> Option<DateTime<Utc>> {
    match zone.from_local_datetime(local) {
        // Spring-forward gap: the local time does not exist, so it is skipped.
        LocalResult::None => None,
        LocalResult::Single(instant) => Some(instant.with_timezone(&Utc)),
        // Fall-back overlap: the earlier instant is the occurrence identity.
        LocalResult::Ambiguous(first, second) => {
            let first = first.with_timezone(&Utc);
            let second = second.with_timezone(&Utc);
            Some(first.min(second))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parses_and_canonicalizes_whitespace() {
        let expression = CronExpression::parse("  0\t*/5   * * MON-FRI ").unwrap();
        assert_eq!(expression.as_str(), "0 */5 * * MON-FRI");
    }

    #[test]
    fn rejects_the_wrong_number_of_fields() {
        assert_eq!(
            CronExpression::parse("0 * * *").unwrap_err(),
            ScheduleError::FieldCount { found: 4 }
        );
        assert_eq!(
            CronExpression::parse("0 0 * * * *").unwrap_err(),
            ScheduleError::FieldCount { found: 6 }
        );
    }

    #[test]
    fn rejects_empty_lists_and_bad_values() {
        assert!(matches!(
            CronExpression::parse("0,,5 * * * *"),
            Err(ScheduleError::EmptyField { field: "minute" })
        ));
        assert!(matches!(
            CronExpression::parse("60 * * * *"),
            Err(ScheduleError::InvalidField {
                field: "minute",
                ..
            })
        ));
        assert!(matches!(
            CronExpression::parse("0 24 * * *"),
            Err(ScheduleError::InvalidField { field: "hour", .. })
        ));
        assert!(matches!(
            CronExpression::parse("0 0 0 * *"),
            Err(ScheduleError::InvalidField {
                field: "day-of-month",
                ..
            })
        ));
        assert!(matches!(
            CronExpression::parse("0 0 * 13 *"),
            Err(ScheduleError::InvalidField { field: "month", .. })
        ));
        assert!(matches!(
            CronExpression::parse("0 0 * * 8"),
            Err(ScheduleError::InvalidField {
                field: "day-of-week",
                ..
            })
        ));
    }

    #[test]
    fn rejects_reversed_ranges_and_zero_steps() {
        assert!(matches!(
            CronExpression::parse("10-5 * * * *"),
            Err(ScheduleError::ReversedRange {
                field: "minute",
                ..
            })
        ));
        assert!(matches!(
            CronExpression::parse("*/0 * * * *"),
            Err(ScheduleError::ZeroStep { field: "minute" })
        ));
        assert!(matches!(
            CronExpression::parse("*/x * * * *"),
            Err(ScheduleError::InvalidField {
                field: "minute",
                ..
            })
        ));
        assert!(matches!(
            CronExpression::parse("*/5/2 * * * *"),
            Err(ScheduleError::InvalidField {
                field: "minute",
                ..
            })
        ));
    }

    #[test]
    fn expands_lists_ranges_steps_and_names() {
        let expression = CronExpression::parse("0,15,30 9-17/4 * JAN-MAR MON,WED,FRI").unwrap();
        assert_eq!(expression.minutes().collect::<Vec<_>>(), vec![0, 15, 30]);
        assert_eq!(expression.hours().collect::<Vec<_>>(), vec![9, 13, 17]);
        let date = NaiveDate::from_ymd_opt(2026, 2, 2).unwrap(); // Monday
        assert!(expression.matches_date(date));
        let date = NaiveDate::from_ymd_opt(2026, 2, 3).unwrap(); // Tuesday
        assert!(!expression.matches_date(date));
        let date = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(); // Wednesday, April
        assert!(!expression.matches_date(date));
    }

    #[test]
    fn steps_from_a_value_run_to_the_end_of_the_field() {
        let expression = CronExpression::parse("0/20 * * * *").unwrap();
        assert_eq!(expression.minutes().collect::<Vec<_>>(), vec![0, 20, 40]);
    }

    #[test]
    fn both_day_fields_restricted_uses_either_rule() {
        // The 1st of the month OR a Monday: classic Vixie cron behavior.
        let expression = CronExpression::parse("0 0 1 * MON").unwrap();
        assert!(expression.matches_date(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()));
        assert!(expression.matches_date(NaiveDate::from_ymd_opt(2026, 6, 8).unwrap()));
        assert!(!expression.matches_date(NaiveDate::from_ymd_opt(2026, 6, 9).unwrap()));
    }

    #[test]
    fn a_single_restricted_day_field_is_used_alone() {
        let sundays = CronExpression::parse("0 0 * * SUN").unwrap();
        assert!(sundays.matches_date(NaiveDate::from_ymd_opt(2026, 6, 7).unwrap()));
        assert!(!sundays.matches_date(NaiveDate::from_ymd_opt(2026, 6, 8).unwrap()));
        let seventh = CronExpression::parse("0 0 7 * *").unwrap();
        assert!(seventh.matches_date(NaiveDate::from_ymd_opt(2026, 6, 7).unwrap()));
        assert!(!seventh.matches_date(NaiveDate::from_ymd_opt(2026, 6, 8).unwrap()));
    }

    #[test]
    fn sunday_is_zero_and_seven() {
        let zero = CronExpression::parse("0 0 * * 0").unwrap();
        let seven = CronExpression::parse("0 0 * * 7").unwrap();
        let sunday = NaiveDate::from_ymd_opt(2026, 6, 7).unwrap();
        assert!(zero.matches_date(sunday));
        assert!(seven.matches_date(sunday));
        assert!(!zero.matches_date(NaiveDate::from_ymd_opt(2026, 6, 8).unwrap()));
        assert!(!seven.matches_date(NaiveDate::from_ymd_opt(2026, 6, 8).unwrap()));
    }

    #[test]
    fn next_occurrence_is_strictly_after_the_given_instant() {
        let schedule = CronSchedule::parse("*/15 * * * *", "UTC").unwrap();
        assert_eq!(
            schedule
                .next_occurrence(at("2026-10-21T12:07:00Z"))
                .unwrap(),
            at("2026-10-21T12:15:00Z")
        );
        assert_eq!(
            schedule
                .next_occurrence(at("2026-10-21T12:15:00Z"))
                .unwrap(),
            at("2026-10-21T12:30:00Z")
        );
    }

    #[test]
    fn next_occurrence_uses_the_schedule_time_zone() {
        let schedule = CronSchedule::parse("0 9 * * *", "Europe/Berlin").unwrap();
        // 08:00Z is 10:00 CEST, so the next 09:00 local time is tomorrow.
        assert_eq!(
            schedule
                .next_occurrence(at("2026-10-21T08:00:00Z"))
                .unwrap(),
            at("2026-10-22T07:00:00Z")
        );
    }

    #[test]
    fn spring_forward_gap_times_are_skipped() {
        // New York jumps from 02:00 EST to 03:00 EDT on 2026-03-08.
        let schedule = CronSchedule::parse("30 2 * * *", "America/New_York").unwrap();
        assert_eq!(
            schedule
                .next_occurrence(at("2026-03-07T12:00:00Z"))
                .unwrap(),
            at("2026-03-09T06:30:00Z")
        );
    }

    #[test]
    fn fall_back_repeats_a_local_time_once_at_the_earlier_instant() {
        // New York repeats 01:30 on 2026-11-01: 05:30Z (EDT) and 06:30Z (EST).
        let schedule = CronSchedule::parse("30 1 * * *", "America/New_York").unwrap();
        assert_eq!(
            schedule
                .next_occurrence(at("2026-10-31T12:00:00Z"))
                .unwrap(),
            at("2026-11-01T05:30:00Z")
        );
        // The repeated reading is not a second occurrence: after the earlier
        // instant, the next start is the following day.
        assert_eq!(
            schedule
                .next_occurrence(at("2026-11-01T05:30:00Z"))
                .unwrap(),
            at("2026-11-02T06:30:00Z")
        );
        assert_eq!(
            schedule
                .next_occurrence(at("2026-11-01T06:00:00Z"))
                .unwrap(),
            at("2026-11-02T06:30:00Z")
        );
    }

    #[test]
    fn leap_day_occurs_in_a_leap_year() {
        let schedule = CronSchedule::parse("0 0 29 2 *", "UTC").unwrap();
        assert_eq!(
            schedule
                .next_occurrence(at("2026-03-01T00:00:00Z"))
                .unwrap(),
            at("2028-02-29T00:00:00Z")
        );
    }

    #[test]
    fn a_non_leap_century_does_not_hide_the_next_leap_day() {
        // 2100 is not a leap year, so the next 29 February after 2096 is 2104.
        let schedule = CronSchedule::parse("0 0 29 2 *", "UTC").unwrap();
        assert_eq!(
            schedule
                .next_occurrence(at("2096-03-01T00:00:00Z"))
                .unwrap(),
            at("2104-02-29T00:00:00Z")
        );
    }

    #[test]
    fn an_impossible_expression_has_no_occurrence() {
        let schedule = CronSchedule::parse("0 0 30 2 *", "UTC").unwrap();
        assert_eq!(schedule.next_occurrence(at("2026-01-01T00:00:00Z")), None);
    }

    #[test]
    fn southern_hemisphere_transitions_use_the_same_rules() {
        // Sydney leaves daylight time at 03:00 on 2026-04-05, repeating 02:30.
        let schedule = CronSchedule::parse("30 2 * * *", "Australia/Sydney").unwrap();
        assert_eq!(
            schedule
                .next_occurrence(at("2026-04-04T12:00:00Z"))
                .unwrap(),
            at("2026-04-04T15:30:00Z"), // 02:30 AEDT, the earlier reading
        );
        assert_eq!(
            schedule
                .next_occurrence(at("2026-04-04T16:00:00Z"))
                .unwrap(),
            at("2026-04-05T16:30:00Z"), // the repeated 02:30 AEST is not a second run
        );
        // Sydney enters daylight time at 02:00 on 2026-10-04, so 02:30 is skipped.
        assert_eq!(
            schedule
                .next_occurrence(at("2026-10-03T12:00:00Z"))
                .unwrap(),
            at("2026-10-04T15:30:00Z"), // 02:30 AEDT on 2026-10-05
        );
    }

    #[test]
    fn abbreviations_are_rejected_because_they_hide_daylight_saving() {
        for abbreviation in ["EST", "MST", "HST", "CET", "GMT", "JST"] {
            assert!(
                matches!(
                    TimeZoneId::parse(abbreviation),
                    Err(ScheduleError::UnknownTimeZone { .. })
                ),
                "{abbreviation} must not parse"
            );
        }
        // Fixed-offset IANA paths stay available when that is what the caller
        // means, and the one abbreviation that is a real zone name is allowed.
        assert!(TimeZoneId::parse("Etc/GMT+5").is_ok());
        assert!(TimeZoneId::parse("UTC").is_ok());
    }

    #[test]
    fn time_zone_names_are_canonicalized() {
        assert_eq!(TimeZoneId::parse("UTC").unwrap().as_str(), "UTC");
        assert_eq!(
            TimeZoneId::parse("Europe/Berlin").unwrap().as_str(),
            "Europe/Berlin"
        );
        assert!(matches!(
            TimeZoneId::parse("Mars/Olympus_Mons"),
            Err(ScheduleError::UnknownTimeZone { .. })
        ));
        assert!(matches!(
            TimeZoneId::parse("utc"),
            Err(ScheduleError::UnknownTimeZone { .. })
        ));
        assert!(matches!(
            TimeZoneId::parse(""),
            Err(ScheduleError::UnknownTimeZone { .. })
        ));
    }

    #[test]
    fn schedule_parse_reports_the_failing_part() {
        assert!(matches!(
            CronSchedule::parse("not a cron", "UTC"),
            Err(ScheduleError::FieldCount { found: 3 })
        ));
        assert!(matches!(
            CronSchedule::parse("0 0 * * *", "Nowhere/Special"),
            Err(ScheduleError::UnknownTimeZone { .. })
        ));
    }
}
