//! Due-schedule admission (BS-019.T2).
//!
//! [`ScheduleRunner`] is the coordinator's tick over persisted schedules. It
//! asks the ledger which schedules are due, admits each in its own transaction,
//! and reports a bounded summary. The runner holds no configuration or table
//! state: everything it needs was validated and persisted by BS-019.T1, and the
//! occurrence row it writes is what makes admission idempotent.

use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::occurrences::{self, OUTSIDE_WINDOW_REASON, OccurrenceState};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// How many due schedules one pass examines. A busy organization is drained
/// across passes; the bound keeps one pass short enough to stay inside the
/// scheduler interval.
pub const DEFAULT_DUE_SCHEDULES: i64 = 16;

/// What one pass did. Counts are diagnostics; durable state is in the ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionSummary {
    /// New jobs created.
    pub admitted: u32,
    /// Evaluations folded into a job that already owns the table.
    pub coalesced: u32,
    /// Evaluations refused because the policy or service identity is disabled.
    pub skipped: u32,
}

/// Admits due schedule occurrences for the active coordinator epoch.
pub struct ScheduleRunner {
    pool: PgPool,
    epoch: i64,
    limit: i64,
    cursor: std::sync::Mutex<Uuid>,
}

impl ScheduleRunner {
    /// Builds a runner bounded to `limit` schedules per pass.
    #[must_use]
    pub fn new(pool: PgPool, epoch: i64, limit: i64) -> Self {
        Self {
            pool,
            epoch,
            limit: limit.clamp(1, 1_000),
            cursor: std::sync::Mutex::new(Uuid::nil()),
        }
    }

    /// Admits everything due at the current instant.
    ///
    /// # Errors
    ///
    /// Returns lost leadership or a database error. A failure scoped to one
    /// schedule, such as a corrupt stored timing, is logged and skipped so the
    /// remaining schedules still run.
    pub async fn admit_due(&self) -> Result<AdmissionSummary, PersistenceError> {
        self.admit_due_with(Utc::now(), Utc::now).await
    }

    /// Admits everything due at `now`. The explicit instant keeps tests and
    /// replay diagnostics deterministic; each admission applies it after the
    /// schedule locks.
    ///
    /// # Errors
    ///
    /// As [`Self::admit_due`].
    pub async fn admit_due_at(
        &self,
        now: DateTime<Utc>,
    ) -> Result<AdmissionSummary, PersistenceError> {
        self.admit_due_with(now, move || now).await
    }

    /// Runs one pass with a separate scan clock and admission clock.
    ///
    /// The scan clock selects due schedules; the admission clock is sampled per
    /// schedule after its row locks, so a pass that blocked cannot admit a run
    /// after its window closed. Production passes `Utc::now` for both.
    ///
    /// # Errors
    ///
    /// As [`Self::admit_due`].
    async fn admit_due_with<F>(
        &self,
        scan_now: DateTime<Utc>,
        clock: F,
    ) -> Result<AdmissionSummary, PersistenceError>
    where
        F: Fn() -> DateTime<Utc> + Send + Sync,
    {
        let mut conn = self.pool.acquire().await?;
        // Schedules that predate the state table (or lost their row) are armed
        // once, before the due scan. This is also the upgrade path from T1.
        occurrences::arm_missing(&mut conn, scan_now, self.limit).await?;
        let cursor = *self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let schedules = occurrences::due_after(&mut conn, scan_now, cursor, self.limit).await?;
        if let Some(last) = schedules.last() {
            *self
                .cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = *last.as_uuid();
        }
        drop(conn);
        let mut summary = AdmissionSummary::default();
        for schedule in schedules {
            let mut conn = self.pool.acquire().await?;
            match occurrences::admit_with_clock(&mut conn, schedule, self.epoch, &clock).await {
                Ok(Some(occurrence)) => match occurrence.state {
                    OccurrenceState::Admitted => {
                        summary.admitted += 1;
                        tracing::info!(
                            schedule_id = %schedule,
                            job_id = ?occurrence.job_id,
                            scheduled_for = %occurrence.scheduled_for,
                            coalesced_until = ?occurrence.coalesced_until,
                            "schedule occurrence admitted"
                        );
                    }
                    OccurrenceState::Coalesced => {
                        summary.coalesced += 1;
                        tracing::debug!(
                            schedule_id = %schedule,
                            job_id = ?occurrence.job_id,
                            reason = occurrence.reason.as_deref().unwrap_or_default(),
                            "schedule occurrence coalesced into an active table job"
                        );
                    }
                    OccurrenceState::Skipped => {
                        summary.skipped += 1;
                        // A tick outside the window is expected for a cron more
                        // frequent than its window; only an operator-actionable
                        // denial deserves a warning.
                        if occurrence.reason.as_deref() == Some(OUTSIDE_WINDOW_REASON) {
                            tracing::debug!(
                                schedule_id = %schedule,
                                "schedule occurrence skipped outside its maintenance window"
                            );
                        } else {
                            tracing::warn!(
                                schedule_id = %schedule,
                                reason = occurrence.reason.as_deref().unwrap_or_default(),
                                "schedule occurrence skipped"
                            );
                        }
                    }
                    OccurrenceState::Pending => {}
                },
                Ok(None) => {}
                Err(error) if is_infrastructure(&error) => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        schedule_id = %schedule,
                        reason = %error,
                        "schedule occurrence admission skipped"
                    );
                }
            }
        }
        Ok(summary)
    }
}

/// Whether the failure concerns the coordinator's ledger, not one schedule.
fn is_infrastructure(error: &PersistenceError) -> bool {
    matches!(
        error,
        PersistenceError::LeadershipLost { .. }
            | PersistenceError::Database(_)
            | PersistenceError::Migration(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_pass_limit_is_bounded() {
        let pool = PgPool::connect_lazy("postgres://localhost/unused").expect("lazy pool");
        assert_eq!(ScheduleRunner::new(pool.clone(), 1, 0).limit, 1);
        assert_eq!(ScheduleRunner::new(pool.clone(), 1, 10_000).limit, 1_000);
        assert_eq!(ScheduleRunner::new(pool, 1, 16).limit, 16);
    }
}
