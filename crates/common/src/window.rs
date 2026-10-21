//! Maintenance windows: the local time-of-day range in which a run may start
//! (BS-019.T3).
//!
//! A window belongs to a schedule and names a start and an end minute in the
//! schedule's own IANA time zone. The interval is half-open (`start` is inside,
//! `end` is not) and wraps past midnight when `start` is later than `end`, so
//! `22:00`–`02:00` is one four-hour overnight window.
//!
//! The window is a *start* gate, not a duration limit: admission checks the
//! instant at which the occurrence would create its job. A run that starts
//! inside the window may finish after it closes. Because the check uses the
//! wall clock in the schedule's zone, a spring-forward gap shortens a window
//! that spans it and a fall-back overlap lengthens it; no instant is invented
//! or dropped by this module.
//!
//! This type is pure: it reads no clock and performs no I/O. Callers pass the
//! local time or the instant to test.

use chrono::{DateTime, Duration, LocalResult, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// Minutes in one day.
pub const MINUTES_PER_DAY: u16 = 24 * 60;

/// How many local dates [`MaintenanceWindow::next_opening`] examines before
/// reporting that no opening is reachable. A daily window opens on the next
/// date unless a time-zone transition removes that date entirely, which the
/// time-zone database has done only a handful of times in history.
const MAX_OPENING_SEARCH_DAYS: i64 = 366;

/// How many local minutes after the nominal start [`opening_on`] scans when a
/// daylight-saving gap removes the nominal start. The largest historical gap
/// is two hours; the bound also stops a corrupt transition table from looping.
const MAX_OPENING_GAP_MINUTES: u32 = 180;

/// A daily local-time range in which a scheduled run may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaintenanceWindow {
    start_minute: u16,
    end_minute: u16,
}

/// Why a maintenance window was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    /// The start minute was outside one day.
    #[error("window start minute {value} is outside 0..={max}")]
    StartOutOfRange {
        /// The rejected value.
        value: i32,
        /// Largest accepted value.
        max: u16,
    },
    /// The end minute was outside one day.
    #[error("window end minute {value} is outside 0..={max}")]
    EndOutOfRange {
        /// The rejected value.
        value: i32,
        /// Largest accepted value.
        max: u16,
    },
    /// Start and end named the same instant, which describes no time at all.
    #[error("a maintenance window cannot start and end at minute {value}")]
    Empty {
        /// The minute both bounds named.
        value: u16,
    },
    /// Only one of the two persisted bounds was present.
    #[error("a maintenance window needs both bounds or neither")]
    Incomplete,
}

impl MaintenanceWindow {
    /// Builds a window from local wall-clock minutes after midnight.
    ///
    /// `start` is inclusive and `end` is exclusive. A `start` later than `end`
    /// wraps past midnight.
    ///
    /// # Errors
    ///
    /// [`WindowError::StartOutOfRange`] or [`WindowError::EndOutOfRange`] when a
    /// minute is not in `0..=1439`, and [`WindowError::Empty`] when both are
    /// equal. Use no window at all to run at any time.
    pub fn new(start_minute: u16, end_minute: u16) -> Result<Self, WindowError> {
        let max = MINUTES_PER_DAY - 1;
        if start_minute > max {
            return Err(WindowError::StartOutOfRange {
                value: i32::from(start_minute),
                max,
            });
        }
        if end_minute > max {
            return Err(WindowError::EndOutOfRange {
                value: i32::from(end_minute),
                max,
            });
        }
        if start_minute == end_minute {
            return Err(WindowError::Empty {
                value: start_minute,
            });
        }
        Ok(Self {
            start_minute,
            end_minute,
        })
    }

    /// Builds a window from two nullable persisted minutes.
    ///
    /// A storage layer reads the two columns together, so this is the one
    /// conversion from its nullable representation. Both bounds present builds
    /// a window, both absent means no window, and one present is corrupt.
    ///
    /// # Errors
    ///
    /// [`WindowError::Incomplete`] when exactly one bound is present, and the
    /// same range and empty-interval errors as [`Self::new`].
    pub fn from_minutes(
        start_minute: Option<i32>,
        end_minute: Option<i32>,
    ) -> Result<Option<Self>, WindowError> {
        let max = MINUTES_PER_DAY - 1;
        match (start_minute, end_minute) {
            (None, None) => Ok(None),
            (Some(start), Some(end)) => {
                let start = u16::try_from(start)
                    .map_err(|_| WindowError::StartOutOfRange { value: start, max })?;
                let end = u16::try_from(end)
                    .map_err(|_| WindowError::EndOutOfRange { value: end, max })?;
                Self::new(start, end).map(Some)
            }
            _ => Err(WindowError::Incomplete),
        }
    }

    /// The inclusive start minute after local midnight.
    #[must_use]
    pub const fn start_minute(self) -> u16 {
        self.start_minute
    }

    /// The exclusive end minute after local midnight.
    #[must_use]
    pub const fn end_minute(self) -> u16 {
        self.end_minute
    }

    /// Whether a local wall-clock time is inside the window.
    ///
    /// Only hours and minutes are compared; seconds and subsecond precision do
    /// not move a time out of the interval it belongs to.
    #[must_use]
    pub fn contains(self, local: chrono::NaiveTime) -> bool {
        // `hour` is 0..=23 and `minute` is 0..=59, so the sum fits in a `u16`.
        let minute = u16::try_from(local.hour() * 60 + local.minute()).unwrap_or(u16::MAX);
        if self.start_minute < self.end_minute {
            minute >= self.start_minute && minute < self.end_minute
        } else {
            // The window wraps midnight: it covers the end of the day and the
            // beginning of the next one.
            minute >= self.start_minute || minute < self.end_minute
        }
    }

    /// Whether `instant` falls inside the window in `zone`.
    ///
    /// The conversion uses the zone's wall clock, so daylight-saving
    /// transitions affect the result exactly as they affect an operator's
    /// clock.
    #[must_use]
    pub fn contains_instant(self, instant: DateTime<Utc>, zone: Tz) -> bool {
        self.contains(instant.with_timezone(&zone).time())
    }

    /// The first opening of this window strictly after `after`, in `zone`.
    ///
    /// A window opens once per local day at `start_minute`. Admission uses this
    /// to skip a whole outside span in one evaluation instead of recording one
    /// skipped occurrence per cron tick. When a spring-forward gap removes the
    /// nominal start, the window opens at the first local minute that exists.
    ///
    /// A fall-back transition repeats the local start. Unlike the cron engine,
    /// which deduplicates to the earlier instant for occurrence identity, the
    /// window is a wall-clock interval: if the earlier reading is already past,
    /// the later reading is still a valid opening and is returned. Otherwise a
    /// run later that morning would be dropped.
    ///
    /// Returns `None` only when no local date within the search horizon has a
    /// resolvable opening.
    #[must_use]
    pub fn next_opening(self, after: DateTime<Utc>, zone: Tz) -> Option<DateTime<Utc>> {
        let first_date = after.with_timezone(&zone).date_naive();
        for offset in 0..=MAX_OPENING_SEARCH_DAYS {
            let date = first_date.checked_add_signed(Duration::days(offset))?;
            if let Some((earlier, later)) = opening_on(date, self.start_minute, zone) {
                if earlier > after {
                    return Some(earlier);
                }
                if let Some(later) = later
                    && later > after
                {
                    return Some(later);
                }
            }
        }
        None
    }
}

/// Resolves one local date's window opening, stepping past a spring-forward gap.
///
/// Returns the earlier reading and, for a fall-back overlap, the later reading
/// of the same local start.
fn opening_on(
    date: NaiveDate,
    start_minute: u16,
    zone: Tz,
) -> Option<(DateTime<Utc>, Option<DateTime<Utc>>)> {
    for extra in 0..MAX_OPENING_GAP_MINUTES {
        let minute = u32::from(start_minute) + extra;
        let hour = minute / 60;
        if hour >= u32::from(MINUTES_PER_DAY / 60) {
            return None;
        }
        let local = date.and_hms_opt(hour, minute % 60, 0)?;
        match zone.from_local_datetime(&local) {
            LocalResult::None => {}
            LocalResult::Single(instant) => return Some((instant.with_timezone(&Utc), None)),
            LocalResult::Ambiguous(earlier, later) => {
                return Some((earlier.with_timezone(&Utc), Some(later.with_timezone(&Utc))));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn time(hour: u32, minute: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(hour, minute, 0).expect("a valid wall-clock time")
    }

    fn instant(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("a valid instant")
    }

    #[test]
    fn bounds_are_one_day_wide_and_not_empty() {
        assert_eq!(
            MaintenanceWindow::new(0, 1439).map(MaintenanceWindow::start_minute),
            Ok(0)
        );
        assert!(matches!(
            MaintenanceWindow::new(1440, 0),
            Err(WindowError::StartOutOfRange { max: 1439, .. })
        ));
        assert!(matches!(
            MaintenanceWindow::new(0, 1440),
            Err(WindowError::EndOutOfRange { max: 1439, .. })
        ));
        assert!(matches!(
            MaintenanceWindow::new(u16::MAX, 0),
            Err(WindowError::StartOutOfRange { .. })
        ));
        assert!(matches!(
            MaintenanceWindow::new(60, 60),
            Err(WindowError::Empty { value: 60 })
        ));
    }

    #[test]
    fn a_same_day_window_includes_its_start_and_excludes_its_end() {
        let window = MaintenanceWindow::new(120, 240).expect("a two-hour window");
        assert!(!window.contains(time(1, 59)));
        assert!(window.contains(time(2, 0)));
        assert!(window.contains(time(3, 59)));
        assert!(!window.contains(time(4, 0)));
        assert!(!window.contains(time(23, 0)));
    }

    #[test]
    fn a_window_may_wrap_past_midnight() {
        let window = MaintenanceWindow::new(22 * 60, 2 * 60).expect("an overnight window");
        assert!(window.contains(time(22, 0)));
        assert!(window.contains(time(23, 59)));
        assert!(window.contains(time(0, 0)));
        assert!(window.contains(time(1, 59)));
        assert!(!window.contains(time(2, 0)));
        assert!(!window.contains(time(21, 59)));
        assert!(!window.contains(time(12, 0)));
    }

    #[test]
    fn nullable_persisted_minutes_round_trip_or_report_incomplete() {
        let window = MaintenanceWindow::new(120, 240).expect("a valid window");
        assert_eq!(MaintenanceWindow::from_minutes(None, None), Ok(None));
        assert_eq!(
            MaintenanceWindow::from_minutes(Some(120), Some(240)),
            Ok(Some(window))
        );
        assert_eq!(
            MaintenanceWindow::from_minutes(Some(120), None),
            Err(WindowError::Incomplete)
        );
        assert_eq!(
            MaintenanceWindow::from_minutes(None, Some(240)),
            Err(WindowError::Incomplete)
        );
        assert_eq!(
            MaintenanceWindow::from_minutes(Some(-1), Some(240)),
            Err(WindowError::StartOutOfRange {
                value: -1,
                max: 1439
            })
        );
        assert_eq!(
            MaintenanceWindow::from_minutes(Some(120), Some(70_000)),
            Err(WindowError::EndOutOfRange {
                value: 70_000,
                max: 1439
            })
        );
        assert_eq!(
            MaintenanceWindow::from_minutes(Some(2_000), Some(2_001)),
            Err(WindowError::StartOutOfRange {
                value: 2_000,
                max: 1439
            })
        );
        assert_eq!(
            MaintenanceWindow::from_minutes(Some(60), Some(60)),
            Err(WindowError::Empty { value: 60 })
        );
    }

    #[test]
    fn the_next_opening_is_strictly_after_the_instant() {
        let window = MaintenanceWindow::new(120, 240).expect("a two-hour window");
        // 01:00 UTC is before today's opening.
        assert_eq!(
            window.next_opening(instant(2026, 5, 1, 1, 0), chrono_tz::UTC),
            Some(instant(2026, 5, 1, 2, 0))
        );
        // The opening itself is not strictly after.
        assert_eq!(
            window.next_opening(instant(2026, 5, 1, 2, 0), chrono_tz::UTC),
            Some(instant(2026, 5, 2, 2, 0))
        );
        // 10:00 UTC is after today's window closed.
        assert_eq!(
            window.next_opening(instant(2026, 5, 1, 10, 0), chrono_tz::UTC),
            Some(instant(2026, 5, 2, 2, 0))
        );
    }

    #[test]
    fn a_wrapping_window_opens_later_the_same_day_or_the_next_one() {
        let window = MaintenanceWindow::new(22 * 60, 2 * 60).expect("an overnight window");
        assert_eq!(
            window.next_opening(instant(2026, 5, 1, 12, 0), chrono_tz::UTC),
            Some(instant(2026, 5, 1, 22, 0))
        );
        assert_eq!(
            window.next_opening(instant(2026, 5, 1, 23, 0), chrono_tz::UTC),
            Some(instant(2026, 5, 2, 22, 0))
        );
    }

    #[test]
    fn a_spring_forward_gap_opens_at_the_first_existing_local_minute() {
        // New York removes 02:00–03:00 on 2026-03-08. A 02:00 start therefore
        // opens at 03:00 EDT, which is 07:00 UTC.
        let window = MaintenanceWindow::new(120, 240).expect("an early-morning window");
        assert_eq!(
            window.next_opening(instant(2026, 3, 8, 6, 0), chrono_tz::America::New_York),
            Some(instant(2026, 3, 8, 7, 0))
        );
    }

    #[test]
    fn a_fall_back_overlap_opens_at_the_earlier_instant() {
        // New York repeats 01:00–02:00 on 2026-11-01. The earlier 01:30 EDT is
        // 05:30 UTC; the later 01:30 EST is 06:30 UTC.
        let window = MaintenanceWindow::new(90, 240).expect("a window containing the overlap");
        assert_eq!(
            window.next_opening(instant(2026, 11, 1, 4, 0), chrono_tz::America::New_York),
            Some(instant(2026, 11, 1, 5, 30))
        );
    }

    #[test]
    fn a_later_fall_back_opening_is_still_ahead() {
        // 06:10 UTC is the second 01:10 EST, after the first 01:30 EDT
        // (05:30 UTC) but before the second 01:30 EST (06:30 UTC). The window
        // is wall-clock based, so the later reading still opens it; returning
        // only the earlier reading would skip the rest of the morning.
        let window = MaintenanceWindow::new(90, 240).expect("a window containing the overlap");
        assert_eq!(
            window.next_opening(instant(2026, 11, 1, 6, 10), chrono_tz::America::New_York),
            Some(instant(2026, 11, 1, 6, 30))
        );
    }

    #[test]
    fn every_minute_of_a_same_day_window_is_classified_once() {
        let window = MaintenanceWindow::new(9 * 60 + 30, 17 * 60).expect("a working-hours window");
        for minute in 0..MINUTES_PER_DAY {
            let local = time(u32::from(minute / 60), u32::from(minute % 60));
            let expected = (570..1020).contains(&minute);
            assert_eq!(window.contains(local), expected, "minute {minute}");
        }
    }

    #[test]
    fn every_minute_of_a_wrapping_window_is_classified_once() {
        let window = MaintenanceWindow::new(22 * 60, 2 * 60).expect("an overnight window");
        for minute in 0..MINUTES_PER_DAY {
            let local = time(u32::from(minute / 60), u32::from(minute % 60));
            let expected = !(120..1320).contains(&minute);
            assert_eq!(window.contains(local), expected, "minute {minute}");
        }
    }

    #[test]
    fn an_instant_is_tested_in_the_schedule_zone() {
        let window = MaintenanceWindow::new(0, 60).expect("the first hour");
        let utc = Utc.with_ymd_and_hms(2026, 5, 1, 15, 30, 0).single();
        let instant = utc.expect("a valid instant");
        // 15:30 UTC is 00:30 in Tokyo (UTC+09:00) and 11:30 in New York.
        assert!(window.contains_instant(instant, chrono_tz::Asia::Tokyo));
        assert!(!window.contains_instant(instant, chrono_tz::America::New_York));
    }

    #[test]
    fn a_spring_forward_gap_leaves_no_local_time_for_the_skipped_hour() {
        // New York moves from 02:00 to 03:00 local on 2026-03-08, so the local
        // hour 02:00–03:00 never happens. A window that names only that hour
        // therefore contains no instant; a wider window still contains the
        // instants on both sides of the gap.
        let gap = MaintenanceWindow::new(120, 180).expect("the skipped local hour");
        let before = Utc.with_ymd_and_hms(2026, 3, 8, 6, 59, 0).single();
        let after = Utc.with_ymd_and_hms(2026, 3, 8, 7, 0, 0).single();
        assert!(!gap.contains_instant(before.expect("an instant"), chrono_tz::America::New_York));
        assert!(!gap.contains_instant(after.expect("an instant"), chrono_tz::America::New_York));

        let wider = MaintenanceWindow::new(60, 240).expect("an early-morning window");
        assert!(wider.contains_instant(before.expect("an instant"), chrono_tz::America::New_York));
        assert!(wider.contains_instant(after.expect("an instant"), chrono_tz::America::New_York));
    }
}
