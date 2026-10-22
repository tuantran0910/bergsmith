//! Operational metrics and readiness (BS-020.T2, ADR 0044).
//!
//! The leader serves Prometheus text on a separate listener. Ledger metrics
//! come from one cached [`LedgerSnapshot`], so concurrent scrapers share a
//! single read per refresh interval and the values survive restarts and
//! leadership changes. Worker RPC latency is process-local. No label or value
//! is taken from a credential, a URL, or free-form text: labels are states,
//! stable reason codes, dispositions, and RPC method names.

mod exposition;
mod rpc;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bergsmith_persistence::Database;
use bergsmith_persistence::metrics::{self, LedgerSnapshot};

use exposition::{CONTENT_TYPE, Exposition, Kind};
pub use rpc::{RpcLatency, RpcLatencyLayer, RpcMetrics};

/// How long a readiness probe waits for PostgreSQL.
const READINESS_TIMEOUT: Duration = Duration::from_secs(2);

const ACTIVE_JOB_STATES: [&str; 7] = [
    "Planning",
    "Queued",
    "Running",
    "ReadyToCommit",
    "Committing",
    "CommitOutcomeUnknown",
    "Cancelling",
];
const ENDED_JOB_STATES: [&str; 5] = ["Succeeded", "NoOp", "Cancelled", "ReplanRequired", "Failed"];
const OUTPUT_OUTCOMES: [&str; 4] = ["published", "conflict", "abandoned", "pending"];
const OCCURRENCE_STATES: [&str; 4] = ["Pending", "Admitted", "Coalesced", "Skipped"];
const SESSION_STATUSES: [&str; 3] = ["live", "draining", "expired"];

/// Ledger snapshot caching and query bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricsConfig {
    /// A snapshot younger than this is served from cache.
    pub refresh: Duration,
    /// Statement timeout for the snapshot, and the wait for a connection.
    pub query_timeout: Duration,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            refresh: Duration::from_secs(15),
            query_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Default)]
struct Cache {
    attempted: Option<Instant>,
    succeeded: bool,
    snapshot: Option<(Instant, LedgerSnapshot)>,
}

/// Shared metrics state for the coordinator process.
#[derive(Clone, Debug)]
pub struct Observability {
    database: Database,
    config: MetricsConfig,
    rpc: RpcMetrics,
    cache: Arc<tokio::sync::Mutex<Cache>>,
}

impl Observability {
    /// Creates the metrics state for `database`.
    #[must_use]
    pub fn new(database: Database, config: MetricsConfig) -> Self {
        Self {
            database,
            config,
            rpc: RpcMetrics::default(),
            cache: Arc::new(tokio::sync::Mutex::new(Cache::default())),
        }
    }

    /// Worker RPC latency histograms; layer the worker server with
    /// [`RpcMetrics::layer`].
    #[must_use]
    pub fn rpc(&self) -> &RpcMetrics {
        &self.rpc
    }

    /// Renders every metric family.
    ///
    /// A failed ledger read is reported as `bergsmith_ledger_up 0` and the
    /// last good snapshot, if any, is still exposed with its age. A failed
    /// read is not retried until the refresh interval passes.
    pub async fn render(&self) -> String {
        let (up, snapshot) = self.snapshot().await;
        let mut out = Exposition::default();
        out.family(
            "bergsmith_build_info",
            Kind::Gauge,
            "Coordinator build; the value is always 1.",
        );
        out.sample(
            "bergsmith_build_info",
            &[("version", env!("CARGO_PKG_VERSION"))],
            1.0,
        );
        out.family(
            "bergsmith_ledger_up",
            Kind::Gauge,
            "Whether the last ledger snapshot read succeeded.",
        );
        out.sample("bergsmith_ledger_up", &[], if up { 1.0 } else { 0.0 });
        if let Some((age, snapshot)) = snapshot {
            out.family(
                "bergsmith_ledger_snapshot_age_seconds",
                Kind::Gauge,
                "Age of the ledger snapshot these values come from.",
            );
            out.sample(
                "bergsmith_ledger_snapshot_age_seconds",
                &[],
                age.as_secs_f64(),
            );
            write_ledger(&mut out, &snapshot);
        }
        self.rpc.write(&mut out);
        out.finish()
    }

    async fn snapshot(&self) -> (bool, Option<(Duration, LedgerSnapshot)>) {
        let mut cache = self.cache.lock().await;
        let stale = cache
            .attempted
            .is_none_or(|attempted| attempted.elapsed() >= self.config.refresh);
        if stale {
            cache.attempted = Some(Instant::now());
            match self.read().await {
                Ok(snapshot) => {
                    cache.succeeded = true;
                    cache.snapshot = Some((Instant::now(), snapshot));
                }
                Err(reason) => {
                    cache.succeeded = false;
                    tracing::warn!(reason, "ledger metrics snapshot failed");
                }
            }
        }
        (
            cache.succeeded,
            cache
                .snapshot
                .as_ref()
                .map(|(taken, snapshot)| (taken.elapsed(), snapshot.clone())),
        )
    }

    async fn read(&self) -> Result<LedgerSnapshot, &'static str> {
        let timeout = self.config.query_timeout;
        let mut conn = tokio::time::timeout(timeout, self.database.pool().acquire())
            .await
            .map_err(|_| "connection_timeout")?
            .map_err(|_| "connection_unavailable")?;
        metrics::ledger_snapshot(&mut conn, timeout)
            .await
            .map_err(|_| "query_failed")
    }
}

/// Whether `database` answers a trivial query within the readiness timeout.
pub(crate) async fn ready(database: &Database) -> bool {
    tokio::time::timeout(READINESS_TIMEOUT, async {
        sqlx::query("SELECT 1")
            .execute(database.pool())
            .await
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

/// Converts a ledger count to a Prometheus value.
#[expect(
    clippy::cast_precision_loss,
    reason = "ledger counts and byte totals are exposed as Prometheus floats"
)]
const fn value(count: i64) -> f64 {
    count as f64
}

/// One family with a single label whose expected values always appear, so a
/// series reads zero instead of disappearing.
fn labeled(
    out: &mut Exposition,
    (name, kind, help): (&str, Kind, &str),
    label: &str,
    keys: &[&str],
    values: &BTreeMap<String, i64>,
) {
    out.family(name, kind, help);
    for key in keys {
        out.sample(
            name,
            &[(label, key)],
            value(*values.get(*key).unwrap_or(&0)),
        );
    }
}

fn write_ledger(out: &mut Exposition, snapshot: &LedgerSnapshot) {
    labeled(
        out,
        (
            "bergsmith_jobs_active",
            Kind::Gauge,
            "Non-terminal jobs by state.",
        ),
        "state",
        &ACTIVE_JOB_STATES,
        &snapshot.active_jobs,
    );
    labeled(
        out,
        (
            "bergsmith_jobs_ended_total",
            Kind::Counter,
            "Jobs that reached a terminal state, by state.",
        ),
        "state",
        &ENDED_JOB_STATES,
        &snapshot.ended_jobs,
    );
    labeled(
        out,
        (
            "bergsmith_output_bytes",
            Kind::Gauge,
            "Accepted rewrite output bytes by job outcome; conflict is work discarded after a conflict.",
        ),
        "outcome",
        &OUTPUT_OUTCOMES,
        &snapshot.output_bytes,
    );
    labeled(
        out,
        (
            "bergsmith_schedule_occurrences",
            Kind::Gauge,
            "Schedule occurrences by state.",
        ),
        "state",
        &OCCURRENCE_STATES,
        &snapshot.schedule_occurrences,
    );
    labeled(
        out,
        (
            "bergsmith_worker_sessions",
            Kind::Gauge,
            "Current worker sessions by lease status.",
        ),
        "status",
        &SESSION_STATUSES,
        &snapshot.worker_sessions,
    );

    out.family(
        "bergsmith_attempt_failures_total",
        Kind::Counter,
        "Attempt failures by stable reason code and failure disposition.",
    );
    for ((reason, disposition), count) in &snapshot.attempt_failures {
        out.sample(
            "bergsmith_attempt_failures_total",
            &[("reason", reason), ("disposition", disposition)],
            value(*count),
        );
    }

    for (name, kind, help, sample) in singles(snapshot) {
        out.family(name, kind, help);
        out.sample(name, &[], sample);
    }
}

/// Unlabeled families, in exposition order.
fn singles(snapshot: &LedgerSnapshot) -> [(&'static str, Kind, &'static str, f64); 14] {
    [
        (
            "bergsmith_tasks_pending",
            Kind::Gauge,
            "Tasks waiting for a worker.",
            value(snapshot.pending_tasks),
        ),
        (
            "bergsmith_task_oldest_pending_seconds",
            Kind::Gauge,
            "How long the longest-waiting pending task has waited (scheduling latency).",
            snapshot.oldest_pending_task_seconds,
        ),
        (
            "bergsmith_tasks_retry_waiting",
            Kind::Gauge,
            "Tasks waiting for a retry timer.",
            value(snapshot.retry_waiting_tasks),
        ),
        (
            "bergsmith_attempts_running",
            Kind::Gauge,
            "Granted or running attempts with a current lease.",
            value(snapshot.running_attempts),
        ),
        (
            "bergsmith_attempts_lease_expired",
            Kind::Gauge,
            "Granted or running attempts whose lease expired and that are not fenced yet.",
            value(snapshot.expired_lease_attempts),
        ),
        (
            "bergsmith_attempts_started_total",
            Kind::Counter,
            "Attempts started; each consumes retry budget.",
            value(snapshot.attempts_started),
        ),
        (
            "bergsmith_planned_input_bytes_total",
            Kind::Counter,
            "Input bytes of every persisted plan.",
            value(snapshot.planned_input_bytes),
        ),
        (
            "bergsmith_retained_unused_output_prefixes",
            Kind::Gauge,
            "Attempt output prefixes of ended jobs that no publication uses; Bergsmith never deletes them.",
            value(snapshot.retained_unused_prefixes),
        ),
        (
            "bergsmith_publications_unresolved",
            Kind::Gauge,
            "Authorized publications without a recorded outcome, including unknown outcomes.",
            value(snapshot.unresolved_publications),
        ),
        (
            "bergsmith_publication_oldest_unresolved_seconds",
            Kind::Gauge,
            "Seconds since the oldest unresolved publication was authorized.",
            snapshot.oldest_unresolved_publication_seconds,
        ),
        (
            "bergsmith_cancellation_requests_total",
            Kind::Counter,
            "Jobs whose cancellation was requested.",
            value(snapshot.cancellation_requests),
        ),
        (
            "bergsmith_cancellation_requests_too_late_total",
            Kind::Counter,
            "Cancellations requested after publication started.",
            value(snapshot.cancellation_requests_too_late),
        ),
        (
            "bergsmith_schedule_occurrences_coalesced_downtime_total",
            Kind::Counter,
            "Occurrences that folded due instants missed during downtime.",
            value(snapshot.coalesced_downtime_occurrences),
        ),
        (
            "bergsmith_schedule_lag_seconds",
            Kind::Gauge,
            "How far the most overdue enabled schedule is past its due instant.",
            snapshot.schedule_lag_seconds,
        ),
    ]
}

/// The metrics listener's router: `GET /metrics` only.
pub fn router(observability: Observability) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(observability)
}

async fn scrape(State(observability): State<Observability>) -> Response {
    (
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        observability.render().await,
    )
        .into_response()
}

/// Readiness for a load balancer: `200` when PostgreSQL answers, else `503`.
pub(crate) async fn readiness(database: &Database) -> StatusCode {
    if ready(database).await {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    fn unreachable() -> Database {
        Database::from_pool(
            PgPoolOptions::new()
                .acquire_timeout(Duration::from_millis(500))
                .connect_lazy("postgres://bergsmith@127.0.0.1:1/bergsmith")
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn an_unreachable_database_is_not_ready_and_reports_ledger_down() {
        let database = unreachable();
        assert_eq!(readiness(&database).await, StatusCode::SERVICE_UNAVAILABLE);

        let observability = Observability::new(database, MetricsConfig::default());
        observability.rpc().observe("Heartbeat", "Ok", 0.01);
        let text = observability.render().await;

        assert!(text.contains("bergsmith_ledger_up 0\n"));
        assert!(!text.contains("bergsmith_jobs_active"));
        assert!(text.contains("bergsmith_build_info{version=\""));
        assert!(text.contains("bergsmith_worker_rpc_duration_seconds_count"));
    }

    #[test]
    fn every_ledger_family_is_written_with_stable_zero_series() {
        let mut snapshot = LedgerSnapshot::default();
        snapshot.active_jobs.insert("Queued".into(), 2);
        snapshot.attempt_failures.insert(
            (
                "ProcessMemoryLimitExceeded".into(),
                "ResourceFailure".into(),
            ),
            1,
        );
        snapshot.schedule_lag_seconds = 1.5;
        let mut out = Exposition::default();

        write_ledger(&mut out, &snapshot);
        let text = out.finish();

        assert!(text.contains("bergsmith_jobs_active{state=\"Queued\"} 2\n"));
        assert!(text.contains("bergsmith_jobs_active{state=\"Planning\"} 0\n"));
        assert!(text.contains("bergsmith_jobs_ended_total{state=\"ReplanRequired\"} 0\n"));
        assert!(text.contains("bergsmith_output_bytes{outcome=\"conflict\"} 0\n"));
        assert!(text.contains("bergsmith_worker_sessions{status=\"expired\"} 0\n"));
        assert!(text.contains(
            "bergsmith_attempt_failures_total{reason=\"ProcessMemoryLimitExceeded\",disposition=\"ResourceFailure\"} 1\n"
        ));
        assert!(text.contains("bergsmith_schedule_lag_seconds 1.5\n"));
        assert_eq!(text.matches("# TYPE ").count(), 20);
    }
}
