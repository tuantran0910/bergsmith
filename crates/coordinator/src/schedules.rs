//! Due-schedule admission (BS-019.T2).
//!
//! [`ScheduleRunner`] is the coordinator's tick over persisted schedules. It
//! asks the ledger which schedules are due, admits each in its own transaction,
//! and reports a bounded summary. The runner holds no configuration or table
//! state: everything it needs was validated and persisted by BS-019.T1, and the
//! occurrence row it writes is what makes admission idempotent.

use bergsmith_maintenance::ThresholdSource;
use bergsmith_maintenance::evaluate;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::occurrences::{
    self, HEALTH_TRIGGER_REASON, HealthEvaluation, OUTSIDE_WINDOW_REASON, OccurrenceState,
};
use bergsmith_persistence::statistics::{self, SAMPLE_VERSION};
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
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
            let verdict = match self.verdict_for(&mut conn, schedule, scan_now).await {
                Ok(verdict) => verdict,
                Err(error) if is_infrastructure(&error) => return Err(error),
                Err(error) => {
                    // A corrupt policy or sample is scoped to its schedule:
                    // log it and let admission treat the trigger as unmet.
                    tracing::warn!(
                        schedule_id = %schedule,
                        reason = %error,
                        "schedule health configuration could not be read"
                    );
                    None
                }
            };
            match occurrences::admit_with_health(
                &mut conn,
                schedule,
                self.epoch,
                &clock,
                verdict.as_ref(),
            )
            .await
            {
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
                        } else if occurrence.reason.as_deref() == Some(HEALTH_TRIGGER_REASON) {
                            tracing::debug!(
                                schedule_id = %schedule,
                                "schedule occurrence skipped because its health trigger is unmet"
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

    /// The health verdict a schedule's policy trigger evaluates.
    ///
    /// Returns `None` when the policy has no trigger, the table has no sample,
    /// the sample predates the health counters, or the sample cannot be
    /// presented. A missing verdict never satisfies a trigger, so admission
    /// skips the occurrence instead of guessing.
    async fn verdict_for(
        &self,
        conn: &mut PgConnection,
        schedule: bergsmith_common::ids::ScheduleId,
        now: DateTime<Utc>,
    ) -> Result<Option<HealthEvaluation>, PersistenceError> {
        let Some(route) = occurrences::schedule_trigger(conn, schedule).await? else {
            return Ok(None);
        };
        let Some(stored) = statistics::latest(conn, route.organization_id, route.table_id).await?
        else {
            return Ok(None);
        };
        if stored.sample_version < SAMPLE_VERSION {
            return Ok(None);
        }
        let confirmed = statistics::last_confirmed_at(conn, route.organization_id, route.table_id)
            .await?
            .unwrap_or(stored.sampled_at);
        let sample = match crate::health::sample_model(&stored) {
            Ok(sample) => sample,
            Err(error) => {
                tracing::warn!(
                    schedule_id = %schedule,
                    error = %error,
                    "health sample cannot be presented; treating the trigger as unmet"
                );
                return Ok(None);
            }
        };
        let health = match evaluate(
            &sample,
            confirmed,
            &route.thresholds,
            ThresholdSource::Policy(route.policy_id),
            now,
        ) {
            Ok(health) => health,
            Err(error) => {
                tracing::warn!(
                    schedule_id = %schedule,
                    error = %error,
                    "health evaluation failed; treating the trigger as unmet"
                );
                return Ok(None);
            }
        };
        Ok(Some(HealthEvaluation {
            verdict: crate::health::verdict(&health),
            policy_revision: route.revision,
        }))
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
