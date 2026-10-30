//! Search, the organization event stream, and Overview deltas (BS-033.T7).
//!
//! Search and the stream read through the lists' grant predicates, so two
//! principals on disjoint grants never find or hear about each other's
//! resources. The stream resumes after any ledger ID without skipping an
//! event that commits out of order.

use super::*;

use bergsmith_common::ids::TableId;
use bergsmith_common::job::JobOperation;
use bergsmith_persistence::jobs::{self, NewJob};

/// A principal holding `role` over `scope`, browsing with `events`.
async fn grantee(
    pool: &PgPool,
    tenant: &support::Tenant,
    scope: GrantScope,
    events: JobEventHub,
) -> Browser {
    let mut conn = pool.acquire().await.unwrap();
    let subject = RoleGrantId::generate().to_string();
    let principal = support::user(&mut conn, tenant.organization, &subject).await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: principal,
            role: Role::Viewer,
            scope,
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);
    Browser::with_events(pool, tenant, principal, &subject, events).await
}

/// A `Planning` job on `table`, created in whatever transaction `conn` is.
async fn job_on(conn: &mut PgConnection, tenant: &support::Tenant, table: TableId) -> JobId {
    let id = JobId::generate();
    jobs::create_job(
        conn,
        &NewJob {
            id,
            organization_id: tenant.organization,
            table_id: table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    id
}

/// The tenant's organization with a second connection, `hidden`, holding
/// `db.secret` and another name, `db.classified`, for the tenant's table.
struct Hidden {
    tenant: support::Tenant,
    connection: CatalogConnectionId,
    secret: TableId,
}

async fn hidden(pool: &PgPool) -> Hidden {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let connection = support::connection(&mut conn, tenant.organization, "hidden").await;
    let secret = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(connection, "secret"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    let table_uuid: Uuid =
        sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id = $1")
            .bind(tenant.table.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(connection, "classified"),
        table_uuid,
    )
    .await
    .unwrap();
    Hidden {
        tenant,
        connection,
        secret,
    }
}

async fn found(browser: &Browser, q: &str) -> Value {
    let (status, body) = browser
        .request("GET", &format!("/api/v1/search?q={q}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{q}: {body}");
    body
}

fn names(results: &Value, kind: &str, key: &str) -> Vec<String> {
    results[kind]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item[key].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn search_finds_only_what_each_grant_shows(pool: PgPool) {
    let Hidden {
        tenant, connection, ..
    } = hidden(&pool).await;
    let main = grantee(
        &pool,
        &tenant,
        GrantScope::connection(tenant.connection),
        event_hub(),
    )
    .await;
    let other = grantee(
        &pool,
        &tenant,
        GrantScope::connection(connection),
        event_hub(),
    )
    .await;

    assert_eq!(
        names(&found(&main, "ai").await, "catalogs", "name"),
        ["main"]
    );
    let results = found(&main, "ent").await;
    assert_eq!(names(&results, "tables", "name"), ["events"]);
    assert!(
        results["principals"].is_null(),
        "only access managers find people"
    );
    for hidden_name in ["classified", "secret", "hidden"] {
        let results = found(&main, hidden_name).await;
        assert!(
            names(&results, "tables", "name").is_empty(),
            "{hidden_name}"
        );
        assert!(
            names(&results, "catalogs", "name").is_empty(),
            "{hidden_name}"
        );
    }

    // The same table is found under the other grant only by its other name.
    let results = found(&other, "c").await;
    assert_eq!(names(&results, "tables", "name"), ["classified", "secret"]);
    assert_eq!(results["tables"][0]["id"], tenant.table.to_string());
    assert_eq!(results["tables"][0]["namespace"], json!(["db"]));
    assert!(names(&found(&other, "events").await, "tables", "name").is_empty());

    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let results = found(&admin, "ali").await;
    assert_eq!(names(&results, "principals", "display_name"), ["alice"]);
    assert_eq!(results["principals"][0]["kind"], "User");
    // Wildcards match themselves, and `limit` bounds each kind.
    assert!(names(&found(&admin, "%25").await, "tables", "name").is_empty());
    let (status, results) = admin
        .request("GET", "/api/v1/search?q=e&limit=1", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(results["tables"].as_array().unwrap().len(), 1);

    let long = "a".repeat(129);
    for query in [
        String::new(),
        "q=".to_owned(),
        format!("q={long}"),
        "q=a&limit=0".to_owned(),
        "q=a&limit=21".to_owned(),
        "q=a&page=1".to_owned(),
    ] {
        let (status, _) = admin
            .request("GET", &format!("/api/v1/search?{query}"), None, None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
}

/// Limits under which a stream polls quickly and nothing else wakes it.
fn polling() -> EventStreamLimits {
    EventStreamLimits {
        poll_interval: Duration::from_millis(20),
        ..quiet_limits()
    }
}

/// One live event: its name, ID, and data.
#[derive(Debug)]
struct Live {
    event: String,
    id: i64,
    data: Value,
}

/// Reads the organization stream's events, checking `job` data against the
/// contract.
struct LiveReader {
    body: BodyDataStream,
    buffer: String,
}

impl LiveReader {
    async fn open(browser: &Browser, uri: &str, last_event_id: Option<&str>) -> Self {
        let response = open_stream(browser, uri, last_event_id).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        Self {
            body: response.into_body().into_data_stream(),
            buffer: String::new(),
        }
    }

    async fn next(&mut self) -> Live {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if let Some(event) = Self::parse(&block) {
                    return event;
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(20), self.body.next())
                .await
                .unwrap()
                .unwrap();
            self.buffer
                .push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
    }

    /// The next event if one arrives within `wait`.
    async fn within(&mut self, wait: Duration) -> Option<Live> {
        tokio::time::timeout(wait, self.next()).await.ok()
    }

    fn parse(block: &str) -> Option<Live> {
        let (mut id, mut event, mut data) = (None, None, None);
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("id: ") {
                id = Some(value.parse().unwrap());
            } else if let Some(value) = line.strip_prefix("event: ") {
                event = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("data: ") {
                data = Some(serde_json::from_str::<Value>(value).unwrap());
            }
        }
        let (event, data) = (event?, data?);
        if event == "job" {
            let document: Value =
                serde_json::from_str(include_str!("../../src/api/openapi.json")).unwrap();
            check_schema(
                &document,
                &document["components"]["schemas"]["LiveJobEvent"],
                &data,
            );
        } else {
            assert!(["ready", "reset"].contains(&event.as_str()), "{event}");
            assert_eq!(data, json!({}));
        }
        Some(Live {
            event,
            id: id.unwrap(),
            data,
        })
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_stream_names_only_visible_jobs_and_resumes_after_any_id(pool: PgPool) {
    let Hidden { tenant, secret, .. } = hidden(&pool).await;
    let viewer = grantee(
        &pool,
        &tenant,
        GrantScope::connection(tenant.connection),
        hub(polling()),
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    let elsewhere = support::tenant(&mut conn, "elsewhere").await;
    let before = job_on(&mut conn, &tenant, tenant.table).await;
    drop(conn);

    let mut reader = LiveReader::open(&viewer, "/api/v1/events", None).await;
    let ready = reader.next().await;
    assert_eq!(ready.event, "ready");

    let mut conn = pool.acquire().await.unwrap();
    job_on(&mut conn, &tenant, secret).await;
    job_on(&mut conn, &elsewhere, elsewhere.table).await;
    let job = job_on(&mut conn, &tenant, tenant.table).await;
    drop(conn);
    advance(&pool, job, 0, JobState::Queued).await;

    // Jobs created before the stream opened, under another grant, or in
    // another organization never appear.
    let created = reader.next().await;
    assert_eq!(created.event, "job");
    assert_eq!(created.data["job_id"], job.to_string());
    assert_eq!(created.data["table_id"], tenant.table.to_string());
    assert_eq!(created.data["state"], "Planning");
    assert!(created.id > ready.id);
    let queued = reader.next().await;
    assert_eq!(queued.data["job_id"], job.to_string());
    assert_eq!(queued.data["state"], "Queued");
    assert_eq!(queued.data["revision"], 1);
    assert!(queued.id > created.id);

    // A reconnect resumes after the last ID it saw.
    let resume = created.id.to_string();
    let mut resumed = LiveReader::open(&viewer, "/api/v1/events", Some(&resume)).await;
    let ready = resumed.next().await;
    assert_eq!((ready.event.as_str(), ready.id), ("ready", created.id));
    let next = resumed.next().await;
    assert_eq!((next.event.as_str(), next.id), ("job", queued.id));
    // From the start of the ledger, the earlier job appears too.
    let mut replay = LiveReader::open(&viewer, "/api/v1/events?after=0", None).await;
    assert_eq!(replay.next().await.event, "ready");
    assert_eq!(replay.next().await.data["job_id"], before.to_string());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_later_id_waits_for_an_earlier_one_still_in_progress(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let admin = Browser::with_events(&pool, &tenant, tenant.user, "alice", hub(polling())).await;
    let mut reader = LiveReader::open(&admin, "/api/v1/events", None).await;
    assert_eq!(reader.next().await.event, "ready");

    // The first job's event takes its ID now and commits last.
    let mut slow = pool.begin().await.unwrap();
    let first = job_on(&mut slow, &tenant, tenant.table).await;
    let mut conn = pool.acquire().await.unwrap();
    let second = job_on(&mut conn, &tenant, tenant.table).await;
    drop(conn);
    assert!(
        reader.within(Duration::from_millis(500)).await.is_none(),
        "the committed later ID is held back"
    );
    slow.commit().await.unwrap();

    let delivered = [reader.next().await, reader.next().await];
    assert_eq!(delivered[0].data["job_id"], first.to_string());
    assert_eq!(delivered[1].data["job_id"], second.to_string());
    assert!(delivered[0].id < delivered[1].id);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_distant_resume_point_resets_and_sessions_hold_four_streams(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("SELECT setval(pg_get_serial_sequence('job_events', 'id'), 20000)")
        .execute(&mut *conn)
        .await
        .unwrap();
    job_on(&mut conn, &tenant, tenant.table).await;
    drop(conn);
    let admin = Browser::with_events(&pool, &tenant, tenant.user, "alice", hub(polling())).await;

    let mut reader = LiveReader::open(&admin, "/api/v1/events", Some("1")).await;
    assert_eq!(reader.next().await.event, "ready");
    let reset = reader.next().await;
    assert_eq!(reset.event, "reset");
    assert_eq!(reset.id, 20_001);

    let _more = [
        LiveReader::open(&admin, "/api/v1/events", None).await,
        LiveReader::open(&admin, "/api/v1/events", None).await,
        LiveReader::open(&admin, "/api/v1/events", None).await,
    ];
    let (status, body) = admin.request("GET", "/api/v1/events", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["reason"], "event_stream_capacity_exhausted");
    drop(reader);
    let _again = LiveReader::open(&admin, "/api/v1/events", None).await;

    let (status, _) = admin
        .request("GET", "/api/v1/events?after=-1", None, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_overview_carries_the_previous_days_outcomes(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    for (state, hours_ago) in [
        (JobState::Succeeded, 1),
        (JobState::Failed, 30),
        (JobState::ReplanRequired, 40),
        (JobState::Succeeded, 50),
    ] {
        let job = job_on(&mut conn, &tenant, tenant.table).await;
        sqlx::query(
            "UPDATE jobs SET state = $2, updated_at = now() - make_interval(hours => $3)
             WHERE id = $1",
        )
        .bind(job.as_uuid())
        .bind(state.as_str())
        .bind(hours_ago)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, overview) = admin.request("GET", "/api/v1/overview", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let jobs = &overview["jobs"];
    assert_eq!(jobs["ended"], "1");
    assert_eq!(jobs["by_state"]["Succeeded"], "1");
    let previous = &jobs["previous"];
    assert_eq!(previous["ended"], "2", "only the day before counts: {jobs}");
    assert_eq!(previous["by_state"]["Failed"], "1");
    assert_eq!(previous["by_state"]["ReplanRequired"], "1");
    assert_eq!(previous["by_state"]["Succeeded"], "0");
    assert_eq!(previous["conflict_rate_percent"], "50.0");
}
