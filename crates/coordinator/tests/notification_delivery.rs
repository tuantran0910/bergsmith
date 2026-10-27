//! Durable notification delivery and the same-transaction producers (BS-029.T3).

#![expect(clippy::unwrap_used, reason = "test fixtures fail on setup errors")]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use bergsmith_common::ids::{
    CatalogConnectionId, JobId, NotificationDestinationId, OrganizationId, PrincipalId, TableId,
};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_common::notifications::{AllowList, EventKind};
use bergsmith_coordinator::notifications::delivery::{BACKOFF, DeliveryLoop, MAX_ATTEMPTS};
use bergsmith_coordinator::notifications::{DeliveryClient, Settings};
use bergsmith_persistence::jobs::{self, NewJob, Transition};
use bergsmith_persistence::notifications::{self, DeliveryEvent, DeliveryOutcome, DeliveryReport};
use bergsmith_persistence::statistics;
use bergsmith_persistence::{Database, MIGRATOR};
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

async fn secrets(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bergsmith-delivery-{name}-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.unwrap();
    for (file, content) in files {
        tokio::fs::write(dir.join(file), content).await.unwrap();
    }
    dir
}

fn client(hosts: &[&str], secrets: Option<&Path>) -> DeliveryClient {
    let settings = Settings::new(
        AllowList::new(hosts).unwrap(),
        secrets.map(Path::to_path_buf),
        Duration::from_secs(5),
    );
    DeliveryClient::new(&settings).unwrap()
}

async fn destination(
    pool: &PgPool,
    organization: OrganizationId,
    created_by: PrincipalId,
    name: &str,
    enabled: bool,
) -> NotificationDestinationId {
    let id = NotificationDestinationId::generate();
    sqlx::query(
        "INSERT INTO notification_destinations
             (id, organization_id, name, kind, url_ref, enabled, created_by)
         VALUES ($1, $2, $3, 'slack', 'slack-url', $4, $5)",
    )
    .bind(id.as_uuid())
    .bind(organization.as_uuid())
    .bind(name)
    .bind(enabled)
    .bind(created_by.as_uuid())
    .execute(pool)
    .await
    .unwrap();
    id
}

#[allow(clippy::too_many_arguments)]
async fn rule(
    pool: &PgPool,
    organization: OrganizationId,
    destination: NotificationDestinationId,
    created_by: PrincipalId,
    name: &str,
    kind: EventKind,
    threshold: Option<i32>,
) {
    sqlx::query(
        "INSERT INTO notification_rules
             (id, organization_id, name, destination_id, event_kind,
              repeat_threshold, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::now_v7())
    .bind(organization.as_uuid())
    .bind(name)
    .bind(destination.as_uuid())
    .bind(kind.as_str())
    .bind(threshold)
    .bind(created_by.as_uuid())
    .execute(pool)
    .await
    .unwrap();
}

fn event(organization: OrganizationId, source: &str, resource: &str) -> DeliveryEvent {
    DeliveryEvent {
        organization_id: organization,
        event_kind: EventKind::JobFailed,
        source_key: source.to_owned(),
        occurred_at: Utc::now(),
        summary: "Job failed".to_owned(),
        resource_kind: "job".to_owned(),
        resource_id: resource.to_owned(),
        resource_label: None,
        reason: Some("Test".to_owned()),
        connection_id: None,
        table_id: None,
        pools: Vec::new(),
    }
}

fn delivery_loop(pool: &PgPool, client: DeliveryClient) -> DeliveryLoop {
    DeliveryLoop::new(Database::from_pool(pool.clone()), client)
}

async fn force_due(pool: &PgPool) {
    sqlx::query(
        "UPDATE notification_deliveries
         SET next_attempt_at = clock_timestamp() - interval '1 second',
             claimed_until = NULL",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn deliveries(pool: &PgPool) -> Vec<(String, i32, String, Option<String>)> {
    sqlx::query_as(
        "SELECT state, attempts, event_kind, last_error
         FROM notification_deliveries ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_matching_rule_enqueues_once_per_source(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    // A rule for another kind must not receive this event.
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "replans",
        EventKind::ReplanRequired,
        None,
    )
    .await;

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
            .await
            .unwrap(),
        0,
        "the same source key inserts nothing twice"
    );
    assert_eq!(deliveries(&pool).await.len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_threshold_counts_occurrences_per_resource(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "replans",
        EventKind::JobFailed,
        Some(2),
    )
    .await;

    let mut conn = pool.acquire().await.unwrap();
    let mut second = event(tenant.organization, "job:2", "job-1");
    second.event_kind = EventKind::JobFailed;
    assert_eq!(
        notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
            .await
            .unwrap(),
        0
    );
    assert!(deliveries(&pool).await.is_empty());
    assert_eq!(
        notifications::enqueue_event(&mut conn, &second)
            .await
            .unwrap(),
        1,
        "the second occurrence reaches the threshold"
    );
    assert_eq!(deliveries(&pool).await.len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_transport_failure_retries_with_backoff_and_then_exhausts(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let dir = secrets("unreachable", &[("slack-url", "https://127.0.0.1:1/hook")]).await;
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
        .await
        .unwrap();
    drop(conn);

    let loop_ = delivery_loop(&pool, client(&["127.0.0.1:1"], Some(&dir)));
    for attempt in 1..=MAX_ATTEMPTS {
        force_due(&pool).await;
        loop_.pass().await.unwrap();
        let rows = deliveries(&pool).await;
        assert_eq!(rows[0].1, attempt);
        assert_eq!(rows[0].3.as_deref(), Some("notification_delivery_failed"));
        let expected = if attempt == MAX_ATTEMPTS {
            "Exhausted"
        } else {
            "Pending"
        };
        assert_eq!(rows[0].0, expected, "after attempt {attempt}");
    }
    let attempts: Vec<(String, i32)> =
        sqlx::query_as("SELECT outcome, attempt FROM notification_attempts ORDER BY attempt")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(attempts.len(), usize::try_from(MAX_ATTEMPTS).unwrap());
    assert!(attempts.iter().all(|(outcome, _)| outcome == "Failed"));
    assert_eq!(BACKOFF.len(), 4);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_disabled_destination_is_not_sent(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", false).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    // Enqueue while enabled, then disable it before the send.
    sqlx::query("UPDATE notification_destinations SET enabled = true WHERE id = $1")
        .bind(destination.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
        .await
        .unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:2", "job-1"))
        .await
        .unwrap();
    drop(conn);
    sqlx::query("UPDATE notification_destinations SET enabled = false WHERE id = $1")
        .bind(destination.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

    let loop_ = delivery_loop(&pool, client(&[], None));
    loop_.pass().await.unwrap();
    let rows = deliveries(&pool).await;
    // No request is made for either delivery; they retry in case the
    // destination is enabled again, and exhaust under the usual cap.
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|(state, attempts, _, reason)| {
            state == "Pending"
                && *attempts == 1
                && reason.as_deref() == Some("destination_disabled")
        }),
        "{rows:?}"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_stale_claim_is_claimed_again(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
        .await
        .unwrap();
    let first = notifications::claim_due(&mut conn, 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        notifications::claim_due(&mut conn, 10)
            .await
            .unwrap()
            .is_empty(),
        "a live claim is not claimed again"
    );
    sqlx::query("UPDATE notification_deliveries SET claimed_until = clock_timestamp() - interval '1 second'")
        .execute(&mut *conn)
        .await
        .unwrap();
    let second = notifications::claim_due(&mut conn, 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_ne!(first.claim_token, second.claim_token);

    // The old sender's completion is stale and changes nothing.
    assert!(
        !notifications::record_delivery(
            &mut conn,
            first.id,
            first.claim_token,
            &DeliveryReport {
                outcome: DeliveryOutcome::Delivered,
                status: Some(200),
                reason: None,
                state: "Delivered",
                next_attempt_at: None,
            },
        )
        .await
        .unwrap()
    );
    let rows = deliveries(&pool).await;
    assert_eq!(rows[0].0, "Pending");
    assert_eq!(rows[0].1, 0);

    // The current owner completes it.
    assert!(
        notifications::record_delivery(
            &mut conn,
            second.id,
            second.claim_token,
            &DeliveryReport {
                outcome: DeliveryOutcome::Delivered,
                status: Some(200),
                reason: None,
                state: "Delivered",
                next_attempt_at: None,
            },
        )
        .await
        .unwrap()
    );
    assert_eq!(deliveries(&pool).await[0].0, "Delivered");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_failed_job_enqueues_a_delivery_in_its_transaction(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let table = TableId::generate();
    sqlx::query(
        "INSERT INTO table_identities (id, organization_id, table_uuid)
         VALUES ($1, $2, $3)",
    )
    .bind(table.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(Uuid::now_v7())
    .execute(&mut *conn)
    .await
    .unwrap();
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    // A pool-filtered rule plus the durable dispatch pool on one task.
    sqlx::query(
        "INSERT INTO notification_rules
             (id, organization_id, name, destination_id, event_kind,
              worker_pool, created_by)
         VALUES ($1, $2, 'default pool', $3, 'job.failed', 'default', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.organization.as_uuid())
    .bind(destination.as_uuid())
    .bind(tenant.user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let job = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: tenant.organization,
            table_id: table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    let plan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO plans (id, job_id, content, content_hash)
         VALUES ($1, $2, '{}'::jsonb, repeat('a', 64))",
    )
    .bind(plan_id)
    .bind(job.id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let task_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tasks (id, job_id, plan_id, ordinal, state)
         VALUES ($1, $2, $3, 0, 'Pending')",
    )
    .bind(task_id)
    .bind(job.id.as_uuid())
    .bind(plan_id)
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dispatch_specs
             (task_id, pool, capability, artifact_uri, artifact_sha256,
              envelope_bytes, memory_bytes, cpu_millis)
         VALUES ($1, 'default', 'compact', 's3://artifact', $2, $3, 1, 1)",
    )
    .bind(task_id)
    .bind(vec![7_u8; 32])
    .bind(vec![1_u8])
    .execute(&mut *conn)
    .await
    .unwrap();
    jobs::transition_job(
        &mut conn,
        &Transition {
            job_id: job.id,
            expected_revision: 0,
            to: JobState::Failed,
            reason: Some("Test".to_owned()),
            actor: None,
            non_publication_proven: false,
        },
    )
    .await
    .unwrap();

    // Both the unfiltered rule and the default-pool rule match.
    let rows = deliveries(&pool).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|(_, _, kind, _)| kind == "job.failed"));
    let sources: Vec<String> = sqlx::query_scalar("SELECT source_key FROM notification_deliveries")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        sources
            .iter()
            .all(|source| *source == format!("job:{}:1", job.id))
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn pruning_removes_only_old_finished_deliveries(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE notification_deliveries
         SET state = 'Delivered', updated_at = clock_timestamp() - interval '40 days'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        notifications::prune_deliveries(&mut conn, Utc::now() - chrono::Duration::days(30))
            .await
            .unwrap(),
        1
    );
    assert!(deliveries(&pool).await.is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn critical_alerts_fire_only_on_entry(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let table = TableId::generate();
    sqlx::query(
        "INSERT INTO table_identities (id, organization_id, table_uuid)
         VALUES ($1, $2, $3)",
    )
    .bind(table.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(Uuid::now_v7())
    .execute(&mut *conn)
    .await
    .unwrap();
    let at = Utc::now();
    assert!(
        !statistics::record_health_state(&mut conn, tenant.organization, table, None, at)
            .await
            .unwrap()
    );
    assert!(
        statistics::record_health_state(
            &mut conn,
            tenant.organization,
            table,
            Some("critical"),
            at
        )
        .await
        .unwrap(),
        "the first Critical observation alerts"
    );
    assert!(
        !statistics::record_health_state(
            &mut conn,
            tenant.organization,
            table,
            Some("critical"),
            at
        )
        .await
        .unwrap(),
        "a continuously Critical table does not alert again"
    );
    assert!(
        !statistics::record_health_state(
            &mut conn,
            tenant.organization,
            table,
            Some("warning"),
            at
        )
        .await
        .unwrap()
    );
    assert!(
        statistics::record_health_state(
            &mut conn,
            tenant.organization,
            table,
            Some("critical"),
            at
        )
        .await
        .unwrap(),
        "recovery allows a later transition to alert"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn overdue_unknown_publications_age_from_the_send(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let table = TableId::generate();
    sqlx::query(
        "INSERT INTO table_identities (id, organization_id, table_uuid)
         VALUES ($1, $2, $3)",
    )
    .bind(table.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(Uuid::now_v7())
    .execute(&mut *conn)
    .await
    .unwrap();
    let connection = CatalogConnectionId::generate();
    sqlx::query(
        "INSERT INTO catalog_connections
             (id, organization_id, name, uri, max_unresolved_publication_seconds)
         VALUES ($1, $2, 'lakekeeper', 'https://catalog.example', 3600)",
    )
    .bind(connection.as_uuid())
    .bind(tenant.organization.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let job = JobId::generate();
    sqlx::query(
        "INSERT INTO jobs
             (id, organization_id, table_identity_id, state, state_revision,
              publication_started_at, created_by)
         VALUES ($1, $2, $3, 'CommitOutcomeUnknown', 5,
                 clock_timestamp() - interval '2 hours', $4)",
    )
    .bind(job.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(table.as_uuid())
    .bind(tenant.user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let publication = Uuid::now_v7();
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(publication)
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         VALUES ($1, $2, $3, $4, repeat('a', 64))",
    )
    .bind(job.as_uuid())
    .bind(publication)
    .bind(serde_json::json!({"catalog_connection_id": connection.to_string()}))
    .bind(vec![0_u8])
    .execute(&mut *conn)
    .await
    .unwrap();

    let overdue = notifications::overdue_unknown_publications(&mut conn)
        .await
        .unwrap();
    assert_eq!(overdue.len(), 1);
    assert_eq!(overdue[0].connection_id, connection);
    assert!(overdue[0].pools.is_empty());

    // A durable dispatch pool on one task comes with the sweep event, so a
    // pool-filtered rule receives it.
    let plan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO plans (id, job_id, content, content_hash)
         VALUES ($1, $2, '{}'::jsonb, repeat('a', 64))",
    )
    .bind(plan_id)
    .bind(job.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let task_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tasks (id, job_id, plan_id, ordinal, state)
         VALUES ($1, $2, $3, 0, 'Pending')",
    )
    .bind(task_id)
    .bind(job.as_uuid())
    .bind(plan_id)
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dispatch_specs
             (task_id, pool, capability, artifact_uri, artifact_sha256,
              envelope_bytes, memory_bytes, cpu_millis)
         VALUES ($1, 'default', 'compact', 's3://artifact', $2, $3, 1, 1)",
    )
    .bind(task_id)
    .bind(vec![7_u8; 32])
    .bind(vec![1_u8])
    .execute(&mut *conn)
    .await
    .unwrap();
    let overdue = notifications::overdue_unknown_publications(&mut conn)
        .await
        .unwrap();
    assert_eq!(overdue.len(), 1);
    assert_eq!(overdue[0].pools, vec!["default".to_owned()]);

    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    sqlx::query(
        "INSERT INTO notification_rules
             (id, organization_id, name, destination_id, event_kind,
              worker_pool, created_by)
         VALUES ($1, $2, 'default pool', $3,
                 'publication.commit_outcome_unknown', 'default', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.organization.as_uuid())
    .bind(destination.as_uuid())
    .bind(tenant.user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let loop_ = delivery_loop(&pool, client(&[], None));
    loop_.pass().await.unwrap();
    let rows = deliveries(&pool).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].2, "publication.commit_outcome_unknown");

    // A recent send is not overdue, and the old job's recent row update did
    // not clear its overdue state.
    let recent = JobId::generate();
    sqlx::query(
        "INSERT INTO jobs
             (id, organization_id, table_identity_id, state, state_revision,
              publication_started_at, created_by)
         VALUES ($1, $2, $3, 'CommitOutcomeUnknown', 5,
                 clock_timestamp(), $4)",
    )
    .bind(recent.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(table.as_uuid())
    .bind(tenant.user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let publication = Uuid::now_v7();
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(publication)
        .bind(recent.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         VALUES ($1, $2, $3, $4, repeat('a', 64))",
    )
    .bind(recent.as_uuid())
    .bind(publication)
    .bind(serde_json::json!({"catalog_connection_id": connection.to_string()}))
    .bind(vec![0_u8])
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET updated_at = clock_timestamp() WHERE id = $1")
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let overdue = notifications::overdue_unknown_publications(&mut conn)
        .await
        .unwrap();
    assert_eq!(overdue.len(), 1);
    assert_eq!(overdue[0].job_id, job);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_failed_attempt_insert_rolls_back_the_delivery_update(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
        .await
        .unwrap();
    let claimed = notifications::claim_due(&mut conn, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    // Force the attempt insert to conflict: the delivery update must roll back
    // with it.
    sqlx::query(
        "INSERT INTO notification_attempts (delivery_id, attempt, outcome)
         VALUES ($1, 1, 'Failed')",
    )
    .bind(claimed.id)
    .execute(&mut *conn)
    .await
    .unwrap();
    let result = notifications::record_delivery(
        &mut conn,
        claimed.id,
        claimed.claim_token,
        &DeliveryReport {
            outcome: DeliveryOutcome::Delivered,
            status: Some(200),
            reason: None,
            state: "Delivered",
            next_attempt_at: None,
        },
    )
    .await;
    assert!(result.is_err(), "the attempt insert must fail");
    let rows = deliveries(&pool).await;
    assert_eq!(rows[0].0, "Pending");
    assert_eq!(rows[0].1, 0, "the update rolled back");
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1, "only the injected row remains");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn two_claims_record_independently(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, tenant.organization, tenant.user, "alerts", true).await;
    rule(
        &pool,
        tenant.organization,
        destination,
        tenant.user,
        "failures",
        EventKind::JobFailed,
        None,
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:1", "job-1"))
        .await
        .unwrap();
    notifications::enqueue_event(&mut conn, &event(tenant.organization, "job:2", "job-1"))
        .await
        .unwrap();
    assert_eq!(deliveries(&pool).await.len(), 2);
    for _ in 0..2 {
        let claimed = notifications::claim_due(&mut conn, 1)
            .await
            .unwrap()
            .pop()
            .expect("a due delivery");
        assert!(
            notifications::record_delivery(
                &mut conn,
                claimed.id,
                claimed.claim_token,
                &DeliveryReport {
                    outcome: DeliveryOutcome::Failed,
                    status: None,
                    reason: Some("destination_disabled"),
                    state: "Pending",
                    next_attempt_at: Some(Utc::now() + chrono::Duration::seconds(30)),
                },
            )
            .await
            .unwrap(),
            "claim for {} was rejected",
            claimed.id
        );
    }
    let rows = deliveries(&pool).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(_, attempts, _, _)| *attempts == 1));
}
