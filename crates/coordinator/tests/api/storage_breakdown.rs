//! Ledger-only storage reads, precision, visibility and failed-check retention.
use super::*;
use bergsmith_persistence::storage_breakdown as ledger;
use chrono::Utc;
fn counts() -> Value {
    let mut content = json!({"version":1,"cutoff_ms":1_767_225_600_000_i64});
    for name in [
        "live",
        "retained",
        "metadata",
        "orphan",
        "protected",
        "total",
        "external",
    ] {
        content[name] = json!({"files":1,"bytes":9_007_199_254_740_993_u64});
    }
    content["total"] = json!({"files":5,"bytes":45_035_996_273_704_965_u64});
    content
}
#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_reads_preserve_precision_and_scope_without_catalog_io(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "storage").await;
    let foreign = support::tenant(&mut conn, "storage-foreign").await;
    let epoch = support::leader(&mut conn).await;
    let content = counts();
    ledger::record(
        &mut conn,
        epoch,
        &ledger::Observation {
            organization_id: tenant.organization,
            table_id: tenant.table,
            metadata_location: "file:///private/metadata.json",
            snapshot_id: Some(9_007_199_254_740_993),
            content: &content,
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    ledger::failure(
        &mut conn,
        epoch,
        tenant.organization,
        tenant.table,
        Utc::now(),
        "StorageListingTooLarge",
    )
    .await
    .unwrap();
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/tables/{}/storage", tenant.table);
    let (status, body) = admin.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["latest"]["live"]["bytes"], "9007199254740993");
    assert_eq!(body["latest"]["snapshot_id"], "9007199254740993");
    assert_eq!(body["latest"]["stale"], true);
    assert_eq!(body["failure_reason"], "StorageListingTooLarge");
    assert!(!body.to_string().contains("private"));
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    assert_eq!(
        viewer.request("GET", &path, None, None).await.0,
        StatusCode::OK
    );
    for id in [foreign.table, bergsmith_common::ids::TableId::generate()] {
        assert_eq!(
            viewer
                .request("GET", &format!("/api/v1/tables/{id}/storage"), None, None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    let (status, body) = admin
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}/storage", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tables"], "1");
    assert_eq!(body["assessed"], "1");
    assert_eq!(body["failed"], "1");
    assert_eq!(body["fresh"], "0");
    assert_eq!(body["live"]["bytes"], "9007199254740993");
    assert_eq!(
        admin
            .request(
                "GET",
                &format!("/api/v1/catalogs/{}/storage", foreign.connection),
                None,
                None
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_without_a_sample_is_unassessed_and_stale_epochs_cannot_write(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "unassessed-storage").await;
    let epoch = support::leader(&mut conn).await;
    assert!(
        ledger::failure(
            &mut conn,
            epoch + 1,
            tenant.organization,
            tenant.table,
            Utc::now(),
            "StorageScanTimedOut"
        )
        .await
        .is_err()
    );
    assert!(
        ledger::latest(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
            .is_none()
    );
    let due = ledger::due(&mut conn, Utc::now(), 10).await.unwrap();
    assert_eq!(due.len(), 1);
    ledger::failure(
        &mut conn,
        epoch,
        tenant.organization,
        tenant.table,
        Utc::now(),
        "StorageScanTimedOut",
    )
    .await
    .unwrap();
    assert!(
        ledger::due(&mut conn, Utc::now() - chrono::TimeDelta::hours(1), 10)
            .await
            .unwrap()
            .is_empty()
    );
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}/storage", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["latest"].is_null());
    assert_eq!(body["enabled"], false);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_catalog_totals_deduplicate_aliases_and_filter_table_grants(pool: PgPool) {
    use bergsmith_persistence::tables;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "storage-rollup").await;
    let epoch = support::leader(&mut conn).await;
    let uuid = tables::table_identity(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .table_uuid;
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "second_name"),
        uuid,
    )
    .await
    .unwrap();
    let hidden = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "hidden"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    for table in [tenant.table, hidden] {
        ledger::record(
            &mut conn,
            epoch,
            &ledger::Observation {
                organization_id: tenant.organization,
                table_id: table,
                metadata_location: "file:///metadata",
                snapshot_id: None,
                content: &counts(),
                at: Utc::now(),
            },
        )
        .await
        .unwrap();
    }
    drop(conn);
    let (_, grant) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    let principal: Uuid =
        sqlx::query_scalar("SELECT principal_id FROM principal_role_grants WHERE id=$1")
            .bind(grant.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    let scoped = ledger::totals(
        &mut conn,
        tenant.organization,
        PrincipalId::from_uuid(principal),
        &[Role::Viewer.as_str().to_owned()],
        tenant.connection,
        Utc::now() - chrono::TimeDelta::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(scoped["tables"], "1");
    assert_eq!(scoped["live"]["bytes"], "9007199254740993");
    let all = ledger::totals(
        &mut conn,
        tenant.organization,
        tenant.user,
        &[Role::Administrator.as_str().to_owned()],
        tenant.connection,
        Utc::now() - chrono::TimeDelta::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(all["tables"], "2");
    assert_eq!(all["live"]["bytes"], "18014398509481986");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_freshness_grace_agrees_between_table_and_catalog(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "storage-grace").await;
    let epoch = support::leader(&mut conn).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    for (minutes, stale) in [(90, false), (150, true)] {
        let mut conn = pool.acquire().await.unwrap();
        ledger::record(
            &mut conn,
            epoch,
            &ledger::Observation {
                organization_id: tenant.organization,
                table_id: tenant.table,
                metadata_location: "file:///metadata",
                snapshot_id: None,
                content: &counts(),
                at: Utc::now() - chrono::TimeDelta::minutes(minutes),
            },
        )
        .await
        .unwrap();
        drop(conn);
        let (status, table) = browser
            .request(
                "GET",
                &format!("/api/v1/tables/{}/storage", tenant.table),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{table}");
        assert_eq!(table["latest"]["stale"], stale);
        let (status, catalog) = browser
            .request(
                "GET",
                &format!("/api/v1/catalogs/{}/storage", tenant.connection),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{catalog}");
        assert_eq!(catalog["fresh"], if stale { "0" } else { "1" });
        assert_eq!(catalog["stale"], if stale { "1" } else { "0" });
    }
}
