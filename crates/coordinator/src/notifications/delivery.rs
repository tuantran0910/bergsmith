//! The durable delivery loop (BS-029.T3, ADR 0060).
//!
//! Each pass produces the events the ledger cannot emit in a transaction (an
//! unknown commit outcome past its connection bound, a pool without workers),
//! claims due deliveries with `SKIP LOCKED`, sends each outside any database
//! transaction, records the attempt, and prunes delivered rows after 30 days.
//!
//! Retries use a fixed backoff and stop after five attempts. A claim that never
//! reports back expires after 60 seconds, so a crash between the send and the
//! mark produces at most one duplicate, which receivers deduplicate on the
//! envelope identifier.

use std::time::Duration;

use bergsmith_common::ids::TableId;
use bergsmith_common::notifications::EventKind;
use bergsmith_persistence::notifications::{
    self, ClaimedDelivery, DeliveryEvent, DeliveryOutcome, DeliveryReport,
};
use bergsmith_persistence::{Database, PersistenceError};
use chrono::{DateTime, Utc};
use sqlx::Connection;
use tracing::{debug, info, warn};

use super::{DeliveryClient, DeliveryError, Event, ResourceRef, SecretError, Target, read_secret};

/// A delivery is attempted at most this many times.
pub const MAX_ATTEMPTS: i32 = 5;

/// The wait after the first four failures; the fifth exhausts the delivery.
pub const BACKOFF: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
];

/// How long delivered and exhausted rows are kept.
pub const PRUNE_AFTER_DAYS: i64 = 30;

/// How many deliveries one pass claims.
pub const CLAIM_LIMIT: i64 = 32;

/// The leader-only delivery loop.
#[derive(Clone, Debug)]
pub struct DeliveryLoop {
    database: Database,
    client: DeliveryClient,
}

impl DeliveryLoop {
    /// Builds the loop from the ledger and the configured client.
    #[must_use]
    pub fn new(database: Database, client: DeliveryClient) -> Self {
        Self { database, client }
    }

    /// Runs one pass per interval until shutdown.
    pub async fn run(&self, interval: Duration, shutdown: impl Future<Output = ()> + Send) {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = tick.tick() => {
                    if let Err(error) = self.pass().await {
                        warn!(%error, "notification delivery pass failed");
                    }
                }
            }
        }
    }

    /// Produces loop-observed events, sends due deliveries, and prunes.
    ///
    /// # Errors
    ///
    /// Database errors. A failed send is recorded on the delivery and does not
    /// fail the pass.
    pub async fn pass(&self) -> Result<(), PersistenceError> {
        self.produce().await?;
        self.deliver().await?;
        self.prune().await?;
        Ok(())
    }

    /// Enqueues events only a sweep can observe, atomically per pass.
    async fn produce(&self) -> Result<(), PersistenceError> {
        let mut conn = self.database.pool().acquire().await?;
        let mut tx = conn.begin().await?;
        let hour = Utc::now().timestamp() / 3600;
        for unknown in notifications::overdue_unknown_publications(&mut tx).await? {
            notifications::enqueue_event(
                &mut tx,
                &DeliveryEvent {
                    organization_id: unknown.organization_id,
                    event_kind: EventKind::CommitOutcomeUnknown,
                    source_key: format!("publication:{}:{hour}", unknown.job_id),
                    occurred_at: Utc::now(),
                    summary: format!(
                        "Job {} has an unresolved commit outcome past its connection bound",
                        unknown.job_id
                    ),
                    resource_kind: "job".to_owned(),
                    resource_id: unknown.job_id.to_string(),
                    resource_label: None,
                    reason: Some("CommitOutcomeUnknown".to_owned()),
                    connection_id: Some(unknown.connection_id),
                    table_id: Some(unknown.table_id),
                    pools: unknown.pools,
                },
            )
            .await?;
        }
        for gap in notifications::pools_without_workers(&mut tx).await? {
            notifications::enqueue_event(
                &mut tx,
                &DeliveryEvent {
                    organization_id: gap.organization_id,
                    event_kind: EventKind::PoolWithoutWorkers,
                    source_key: format!("pool:{}:{hour}", gap.pool),
                    occurred_at: Utc::now(),
                    summary: format!(
                        "Worker pool {} has pending work and no live worker",
                        gap.pool
                    ),
                    resource_kind: "worker_pool".to_owned(),
                    resource_id: gap.pool.clone(),
                    resource_label: Some(gap.pool.clone()),
                    reason: Some("PoolWithoutWorkers".to_owned()),
                    connection_id: None,
                    table_id: None,
                    pools: vec![gap.pool],
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Claims and sends due deliveries one at a time, so no claim waits behind
    /// another send and eligibility is read immediately before each request.
    async fn deliver(&self) -> Result<(), PersistenceError> {
        for _ in 0..CLAIM_LIMIT {
            let mut conn = self.database.pool().acquire().await?;
            let mut claimed = notifications::claim_due(&mut conn, 1).await?;
            let Some(delivery) = claimed.pop() else {
                break;
            };
            drop(conn);
            self.send(delivery).await?;
        }
        Ok(())
    }

    /// Sends one claimed delivery and records the result.
    async fn send(&self, delivery: ClaimedDelivery) -> Result<(), PersistenceError> {
        // Re-read eligibility immediately before the request, not from the
        // claim's snapshot.
        let mut conn = self.database.pool().acquire().await?;
        let enabled: Option<bool> = sqlx::query_scalar(
            "SELECT enabled FROM notification_destinations
             WHERE organization_id = $1 AND id = $2",
        )
        .bind(delivery.organization_id.as_uuid())
        .bind(delivery.destination_id.as_uuid())
        .fetch_optional(&mut *conn)
        .await?;
        drop(conn);
        if enabled != Some(true) {
            return self
                .finish(
                    &delivery,
                    DeliveryOutcome::Failed,
                    None,
                    Some("destination_disabled"),
                )
                .await;
        }
        let event = Event {
            id: delivery.id,
            kind: delivery.event_kind.as_str().to_owned(),
            occurred_at: delivery.occurred_at,
            organization_id: delivery.organization_id,
            summary: delivery.summary.clone(),
            resource: ResourceRef {
                kind: delivery.resource_kind.clone(),
                id: delivery.resource_id.clone(),
                label: delivery.resource_label.clone(),
            },
            reason: delivery.reason.clone(),
        };
        let url = match read_secret(self.client.secrets_dir(), &delivery.url_ref).await {
            Ok(secret) => secret.expose().to_owned(),
            Err(error) => {
                let reason = secret_reason(&error);
                return self
                    .finish(&delivery, DeliveryOutcome::Failed, None, Some(reason))
                    .await;
            }
        };
        let target = Target {
            kind: delivery.destination_kind,
            url: &url,
            signing_secret_ref: delivery.signing_secret_ref.as_deref(),
        };
        match self.client.send(&target, &event).await {
            Ok(result) if result.delivered => {
                self.finish(
                    &delivery,
                    DeliveryOutcome::Delivered,
                    Some(result.status),
                    None,
                )
                .await
            }
            Ok(result) => {
                self.finish(
                    &delivery,
                    DeliveryOutcome::Rejected,
                    Some(result.status),
                    Some("destination_rejected"),
                )
                .await
            }
            Err(error) => {
                let reason = delivery_reason(&error);
                self.finish(&delivery, DeliveryOutcome::Failed, None, Some(reason))
                    .await
            }
        }
    }

    /// Records one attempt, computing the next wait or the exhausted state.
    async fn finish(
        &self,
        delivery: &ClaimedDelivery,
        outcome: DeliveryOutcome,
        status: Option<u16>,
        reason: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let attempts = delivery.attempts + 1;
        let (state, next) = match outcome {
            DeliveryOutcome::Delivered => ("Delivered", None),
            _ if attempts >= MAX_ATTEMPTS => ("Exhausted", None),
            _ => {
                let wait = BACKOFF[usize::try_from(attempts - 1)
                    .unwrap_or(0)
                    .min(BACKOFF.len() - 1)];
                (
                    "Pending",
                    Some(Utc::now() + chrono::Duration::from_std(wait).unwrap_or_default()),
                )
            }
        };
        let mut conn = self.database.pool().acquire().await?;
        let recorded = notifications::record_delivery(
            &mut conn,
            delivery.id,
            delivery.claim_token,
            &DeliveryReport {
                outcome,
                status,
                reason,
                state,
                next_attempt_at: next,
            },
        )
        .await?;
        if !recorded {
            debug!(delivery = %delivery.id, "stale notification completion ignored");
            return Ok(());
        }
        if state == "Exhausted" {
            info!(delivery = %delivery.id, "notification delivery exhausted");
        }
        Ok(())
    }

    /// Removes delivered and exhausted deliveries past the retention window.
    async fn prune(&self) -> Result<(), PersistenceError> {
        let cutoff = Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS);
        let mut conn = self.database.pool().acquire().await?;
        notifications::prune_deliveries(&mut conn, cutoff).await?;
        Ok(())
    }
}

fn delivery_reason(error: &DeliveryError) -> &'static str {
    match error {
        DeliveryError::InvalidUrl(_) => "destination_not_allowed",
        DeliveryError::MissingSecret => "signing_secret_missing",
        DeliveryError::Secret(_) => "signing_secret_unavailable",
        DeliveryError::Encode(_) => "payload_encode_failed",
        DeliveryError::Transport { .. } => "notification_delivery_failed",
        DeliveryError::Client { .. } => "notification_client_unavailable",
    }
}

fn secret_reason(error: &SecretError) -> &'static str {
    match error {
        SecretError::NotConfigured => "notifications_secrets_dir_unconfigured",
        SecretError::Unavailable { .. } | SecretError::Empty => "destination_url_unavailable",
        SecretError::InvalidReference | SecretError::NotUtf8 | SecretError::TooLarge { .. } => {
            "destination_url_invalid"
        }
    }
}

/// The instant a table-critical event is deduplicated on.
#[must_use]
pub fn table_source_key(table_id: TableId, at: DateTime<Utc>) -> String {
    format!("table:{table_id}:{}", at.timestamp_millis())
}
