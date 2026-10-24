//! Organizations, principals, catalog connections, and table identities
//! against real PostgreSQL (BS-003.T2).
#![expect(clippy::unwrap_used, reason = "fixtures fail on setup errors")]

mod support;

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::tables::{self, TableAlias};
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use sqlx::PgPool;
use support::{alias, connection, constraint, organization, user};
use uuid::Uuid;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn routing_edits_wait_for_job_creation_and_preserve_its_connection(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let current = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap();
    let new = NewCatalogConnection {
        id: current.id,
        organization_id: current.organization_id,
        name: current.name,
        uri: "https://replacement.example/catalog".into(),
        warehouse: current.warehouse,
        credential_ref: current.credential_ref,
    };
    let mut creating = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *creating)
        .await
        .unwrap();
    support::job(&mut creating, &tenant).await;
    let mut editing = pool.acquire().await.unwrap();
    let waiter: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *editing)
        .await
        .unwrap();
    let edit = tokio::spawn(async move {
        tenancy::update_catalog_connection(
            &mut editing,
            &new,
            0,
            true,
            bergsmith_common::settings::ConnectionSettings::default(),
        )
        .await
    });
    support::wait_until_blocked(&mut conn, waiter, blocker).await;
    creating.commit().await.unwrap();
    assert!(matches!(
        edit.await.unwrap(),
        Err(PersistenceError::CatalogConnectionInUse { .. })
    ));
    assert_eq!(
        tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
            .await
            .unwrap()
            .uri,
        "https://catalog.example/catalog"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn connection_settings_are_stored_and_read_back(pool: PgPool) {
    use bergsmith_common::settings::ConnectionSettings;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let current = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap();
    // A new connection starts with the default bounds and maintenance allowed.
    assert_eq!(current.settings, ConnectionSettings::default());

    let settings = ConnectionSettings::new(600, 7200, false).unwrap();
    let updated = tenancy::update_catalog_connection(
        &mut conn,
        &NewCatalogConnection {
            id: current.id,
            organization_id: current.organization_id,
            name: current.name,
            uri: current.uri,
            warehouse: current.warehouse,
            credential_ref: current.credential_ref,
        },
        current.revision,
        current.enabled,
        settings,
    )
    .await
    .unwrap();

    assert_eq!(updated.settings, settings);
    assert_eq!(updated.revision, current.revision + 1);
    let read = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap();
    assert_eq!(read.settings, settings);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_connection_without_unresolved_publications_reports_none(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;

    let publications =
        tenancy::unresolved_publications(&mut conn, tenant.organization, tenant.connection)
            .await
            .unwrap();

    assert_eq!(publications.count, 0);
    assert_eq!(publications.oldest_seconds, 0);
    assert!(!publications.is_overdue(60));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unresolved_publication_is_reported_against_its_own_connection(pool: PgPool) {
    use bergsmith_common::ids::PublicationId;
    use bergsmith_common::job::JobState;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = connection(&mut conn, tenant.organization, "other").await;
    let job = support::job(&mut conn, &tenant).await;
    let publication = PublicationId::generate();
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(publication.as_uuid())
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    // The intent names the connection that sent the request.
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         VALUES ($1, $2, jsonb_build_object('catalog_connection_id', $3::text), $4, $5)",
    )
    .bind(job.as_uuid())
    .bind(publication.as_uuid())
    .bind(tenant.connection.to_string())
    .bind(Vec::<u8>::new())
    .bind("0".repeat(64))
    .execute(&mut *conn)
    .await
    .unwrap();
    support::force_job_state(&mut conn, job, JobState::Committing).await;
    sqlx::query(
        "UPDATE jobs SET publication_started_at = clock_timestamp() - interval '120 seconds' WHERE id = $1",
    )
    .bind(job.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();

    let publications =
        tenancy::unresolved_publications(&mut conn, tenant.organization, tenant.connection)
            .await
            .unwrap();
    assert_eq!(publications.count, 1);
    assert!(
        publications.oldest_seconds >= 120,
        "the age of the oldest unresolved publication is reported"
    );
    assert!(publications.is_overdue(60));
    assert!(!publications.is_overdue(3600));

    // The publication belongs to the connection that sent it, not to another
    // connection of the same organization.
    let elsewhere = tenancy::unresolved_publications(&mut conn, tenant.organization, other)
        .await
        .unwrap();
    assert_eq!(elsewhere.count, 0);
    assert!(!elsewhere.is_overdue(60));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn organization_slugs_are_unique(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    organization(&mut conn, "acme").await;

    let error = tenancy::create_organization(
        &mut conn,
        &NewOrganization {
            id: OrganizationId::generate(),
            slug: "acme".to_owned(),
            display_name: "Another Acme".to_owned(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(error, PersistenceError::AlreadyExists { .. }));
    assert_eq!(constraint(&error), "organizations_slug_key");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn organization_slugs_must_be_url_safe(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    for slug in ["Acme", "-acme", "acme corp", ""] {
        let error = tenancy::create_organization(
            &mut conn,
            &NewOrganization {
                id: OrganizationId::generate(),
                slug: slug.to_owned(),
                display_name: "Acme".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, PersistenceError::InvalidValue { .. }),
            "{slug:?} should be rejected"
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn organizations_are_found_by_slug(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let id = organization(&mut conn, "acme").await;

    let found = tenancy::organization_by_slug(&mut conn, "acme")
        .await
        .unwrap();
    let missing = tenancy::organization_by_slug(&mut conn, "nobody")
        .await
        .unwrap_err();

    assert_eq!(found.id, id);
    assert_eq!(found.display_name, "acme");
    assert!(matches!(
        missing,
        PersistenceError::NotFound {
            entity: "organization",
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn principal_subjects_are_unique_within_an_organization_only(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let globex = organization(&mut conn, "globex").await;
    user(&mut conn, acme, "alice").await;
    user(&mut conn, globex, "alice").await;

    let error = tenancy::create_principal(
        &mut conn,
        &NewPrincipal {
            id: PrincipalId::generate(),
            organization_id: acme,
            kind: PrincipalKind::Service,
            issuer: "https://idp.example".to_owned(),
            subject: "alice".to_owned(),
            display_name: "duplicate".to_owned(),
        },
    )
    .await
    .unwrap_err();

    assert_eq!(
        constraint(&error),
        "principals_organization_id_issuer_subject_key"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn principals_require_an_existing_organization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    let error = tenancy::create_principal(
        &mut conn,
        &NewPrincipal {
            id: PrincipalId::generate(),
            organization_id: OrganizationId::generate(),
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: "alice".to_owned(),
            display_name: "Alice".to_owned(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::MissingReference {
            entity: "principal",
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn principals_are_found_by_issuer_and_subject(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let id = user(&mut conn, acme, "alice").await;

    let found = tenancy::principal_by_subject(&mut conn, acme, "https://idp.example", "alice")
        .await
        .unwrap();
    let other_issuer =
        tenancy::principal_by_subject(&mut conn, acme, "https://evil.example", "alice")
            .await
            .unwrap_err();

    assert_eq!(found.id, id);
    assert_eq!(found.kind, PrincipalKind::User);
    assert!(matches!(other_issuer, PersistenceError::NotFound { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn connection_names_are_unique_within_an_organization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    connection(&mut conn, acme, "main").await;

    let error = tenancy::create_catalog_connection(
        &mut conn,
        &NewCatalogConnection {
            id: CatalogConnectionId::generate(),
            organization_id: acme,
            name: "main".to_owned(),
            uri: "https://other.example".to_owned(),
            warehouse: None,
            credential_ref: None,
        },
    )
    .await
    .unwrap_err();

    assert_eq!(
        constraint(&error),
        "catalog_connections_organization_id_name_key"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn connection_uris_must_be_http(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;

    let error = tenancy::create_catalog_connection(
        &mut conn,
        &NewCatalogConnection {
            id: CatalogConnectionId::generate(),
            organization_id: acme,
            name: "main".to_owned(),
            uri: "file:///etc/passwd".to_owned(),
            warehouse: None,
            credential_ref: None,
        },
    )
    .await
    .unwrap_err();

    assert_eq!(constraint(&error), "catalog_connections_uri_check");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn connections_are_not_visible_to_other_organizations(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let globex = organization(&mut conn, "globex").await;
    let id = connection(&mut conn, acme, "main").await;

    let own = tenancy::catalog_connection(&mut conn, acme, id)
        .await
        .unwrap();
    let foreign = tenancy::catalog_connection(&mut conn, globex, id)
        .await
        .unwrap_err();

    assert_eq!(own.credential_ref.as_deref(), Some("secret/catalog"));
    assert!(matches!(foreign, PersistenceError::NotFound { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn resolving_a_table_is_idempotent(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let table_uuid = Uuid::now_v7();

    let first = tables::resolve_table(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap();
    let second = tables::resolve_table(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(first.table_uuid, table_uuid);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn aliases_of_the_same_table_share_one_identity(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let replica = connection(&mut conn, acme, "replica").await;
    let table_uuid = Uuid::now_v7();

    let via_main = tables::resolve_table(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap();
    let via_replica =
        tables::resolve_table(&mut conn, acme, &alias(replica, "renamed"), table_uuid)
            .await
            .unwrap();

    assert_eq!(via_main.id, via_replica.id);
    let found = tables::table_by_alias(&mut conn, &alias(replica, "renamed"))
        .await
        .unwrap();
    assert_eq!(found.map(|t| t.id), Some(via_main.id));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_alias_reused_by_a_recreated_table_is_a_conflict(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let original = tables::resolve_table(&mut conn, acme, &alias(main, "events"), Uuid::now_v7())
        .await
        .unwrap();

    let error = tables::resolve_table(&mut conn, acme, &alias(main, "events"), Uuid::now_v7())
        .await
        .unwrap_err();

    let PersistenceError::TableIdentityConflict {
        existing,
        connection,
        ..
    } = error
    else {
        panic!("expected an identity conflict, got {error:?}");
    };
    assert_eq!(existing, original.id);
    assert_eq!(connection, main);
    // The rejected identity was rolled back with the alias write.
    let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM table_identities")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(identities, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registering_an_alias_reports_whether_it_is_new(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let table_uuid = Uuid::now_v7();

    let first = tables::register_alias(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap();
    let second = tables::register_alias(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap();

    assert!(first.created);
    assert_eq!(first.identity, second.identity);
    assert!(!second.created, "the name was registered by the first call");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_namespace_registration_records_every_alias_or_none(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    // The second name already reaches another table, so the request fails.
    tables::resolve_table(&mut conn, acme, &alias(main, "conflict"), Uuid::now_v7())
        .await
        .unwrap();

    let error = tables::register_aliases(
        &mut conn,
        acme,
        &[
            (alias(main, "orders"), Uuid::now_v7()),
            (alias(main, "conflict"), Uuid::now_v7()),
        ],
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, PersistenceError::TableIdentityConflict { .. }),
        "{error:?}"
    );
    assert!(
        tables::table_by_alias(&mut conn, &alias(main, "orders"))
            .await
            .unwrap()
            .is_none(),
        "the first alias is rolled back with the request"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unregistering_keeps_the_table_and_its_other_names(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let replica = connection(&mut conn, acme, "replica").await;
    let table_uuid = Uuid::now_v7();
    let original = tables::resolve_table(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap();
    tables::resolve_table(&mut conn, acme, &alias(replica, "renamed"), table_uuid)
        .await
        .unwrap();

    let revision = alias_revision(&mut conn, replica, "renamed").await;
    let removed = tables::unregister_alias(&mut conn, acme, &alias(replica, "renamed"), revision)
        .await
        .unwrap();
    assert_eq!(removed, original.id);

    assert!(
        tables::table_by_alias(&mut conn, &alias(replica, "renamed"))
            .await
            .unwrap()
            .is_none()
    );
    let kept = tables::table_by_alias(&mut conn, &alias(main, "events"))
        .await
        .unwrap();
    assert_eq!(kept.map(|table| table.id), Some(original.id));

    let missing = tables::unregister_alias(&mut conn, acme, &alias(replica, "renamed"), revision)
        .await
        .unwrap_err();
    assert!(matches!(missing, PersistenceError::NotFound { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unregistering_a_table_with_work_in_flight_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let _job = support::job(&mut conn, &tenant).await;

    let revision = alias_revision(&mut conn, tenant.connection, "events").await;
    let error = tables::unregister_alias(
        &mut conn,
        tenant.organization,
        &alias(tenant.connection, "events"),
        revision,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, PersistenceError::TableInUse { .. }),
        "{error:?}"
    );
    assert!(
        tables::table_by_alias(&mut conn, &alias(tenant.connection, "events"))
            .await
            .unwrap()
            .is_some()
    );
}

/// The revision of one alias row.
async fn alias_revision(
    conn: &mut sqlx::PgConnection,
    connection: CatalogConnectionId,
    name: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT revision FROM table_aliases
         WHERE catalog_connection_id = $1 AND table_name = $2",
    )
    .bind(connection.as_uuid())
    .bind(name)
    .fetch_one(conn)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_stale_removal_does_not_remove_a_replacement(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let name = alias(main, "events");

    // The operator reads the name, and the row it read is removed.
    let observed = tables::register_alias(&mut conn, acme, &name, Uuid::now_v7())
        .await
        .unwrap();
    let revision = alias_revision(&mut conn, main, "events").await;
    tables::unregister_alias(&mut conn, acme, &name, revision)
        .await
        .unwrap();

    // A replacement takes the name before the delayed removal arrives.
    let replacement = tables::register_alias(&mut conn, acme, &name, Uuid::now_v7())
        .await
        .unwrap();
    assert!(replacement.created);
    assert_ne!(replacement.identity.id, observed.identity.id);

    let error = tables::unregister_alias(&mut conn, acme, &name, revision)
        .await
        .unwrap_err();

    assert!(
        matches!(error, PersistenceError::ConfigRevisionConflict { .. }),
        "{error:?}"
    );
    let kept = tables::table_by_alias(&mut conn, &name).await.unwrap();
    assert_eq!(
        kept.map(|table| table.id),
        Some(replacement.identity.id),
        "the replacement survives the stale removal"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registered_aliases_lists_only_the_named_names(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let orders = tables::resolve_table(&mut conn, acme, &alias(main, "orders"), Uuid::now_v7())
        .await
        .unwrap();
    tables::resolve_table(&mut conn, acme, &alias(main, "refunds"), Uuid::now_v7())
        .await
        .unwrap();

    let found = tables::registered_aliases(
        &mut conn,
        acme,
        main,
        &["db".to_owned()],
        &["orders".to_owned(), "missing".to_owned()],
    )
    .await
    .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(
        found.get("orders").map(|name| name.table_id),
        Some(orders.id)
    );
    assert!(found["orders"].revision > 0);
    assert!(
        tables::registered_aliases(&mut conn, acme, main, &["db".to_owned()], &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn aliases_cannot_use_another_organizations_connection(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let globex = organization(&mut conn, "globex").await;
    let acme_main = connection(&mut conn, acme, "main").await;

    let error = tables::resolve_table(
        &mut conn,
        globex,
        &alias(acme_main, "events"),
        Uuid::now_v7(),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::MissingReference {
            entity: "table alias",
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn aliases_need_a_namespace(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let rootless = TableAlias {
        connection_id: main,
        namespace: Vec::new(),
        name: "events".to_owned(),
    };

    let error = tables::resolve_table(&mut conn, acme, &rootless, Uuid::now_v7())
        .await
        .unwrap_err();

    assert_eq!(constraint(&error), "table_aliases_namespace_check");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unknown_aliases_resolve_to_nothing(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;

    let found = tables::table_by_alias(&mut conn, &alias(main, "missing"))
        .await
        .unwrap();

    assert_eq!(found, None);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn accessible_aliases_are_scoped_to_the_grants_and_ordered_canonically(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let globex = organization(&mut conn, "globex").await;
    let main = connection(&mut conn, acme, "main").await;
    let replica = connection(&mut conn, acme, "replica").await;
    let foreign = connection(&mut conn, globex, "foreign").await;
    let table_uuid = Uuid::now_v7();
    let table = tables::resolve_table(&mut conn, acme, &alias(main, "events"), table_uuid)
        .await
        .unwrap()
        .id;
    tables::resolve_table(&mut conn, acme, &alias(replica, "events_v"), table_uuid)
        .await
        .unwrap();
    tables::resolve_table(&mut conn, globex, &alias(foreign, "events"), table_uuid)
        .await
        .unwrap();
    let principal = user(&mut conn, acme, "reader").await;

    let identity = tables::table_identity(&mut conn, acme, table)
        .await
        .unwrap();
    assert_eq!(identity.table_uuid, table_uuid);
    assert!(matches!(
        tables::table_identity(&mut conn, acme, bergsmith_common::ids::TableId::generate())
            .await
            .unwrap_err(),
        PersistenceError::NotFound { .. }
    ));

    // No grant: no catalog name is usable.
    assert!(
        tables::accessible_aliases_with_connections(&mut conn, acme, principal, &["Viewer"], table)
            .await
            .unwrap()
            .is_empty()
    );

    // A connection grant reaches only that connection's name.
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: acme,
            principal_id: principal,
            role: Role::Viewer,
            scope: GrantScope::connection(replica),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let declared =
        tables::accessible_aliases_with_connections(&mut conn, acme, principal, &["Viewer"], table)
            .await
            .unwrap();
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].connection.id, replica);
    assert_eq!(declared[0].alias.name, "events_v");
    assert_eq!(declared[0].alias.namespace, vec!["db".to_owned()]);
    assert_eq!(
        declared[0].connection.credential_ref.as_deref(),
        Some("secret/catalog")
    );
    // A role the grant does not carry sees nothing.
    assert!(
        tables::accessible_aliases_with_connections(
            &mut conn,
            acme,
            principal,
            &["Operator"],
            table
        )
        .await
        .unwrap()
        .is_empty()
    );

    // An organization grant reaches every name of the table in the organization.
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: acme,
            principal_id: principal,
            role: Role::Operator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let all = tables::accessible_aliases_with_connections(
        &mut conn,
        acme,
        principal,
        &["Viewer", "Operator"],
        table,
    )
    .await
    .unwrap();
    assert_eq!(all.len(), 2, "another organization's alias is not listed");
    let names: Vec<&str> = all
        .iter()
        .map(|entry| entry.connection.name.as_str())
        .collect();
    assert!(names.contains(&"main"));
    assert!(names.contains(&"replica"));
    let mut canonically_ordered = all.clone();
    canonically_ordered.sort_by_key(|entry| {
        (
            *entry.alias.connection_id.as_uuid(),
            entry.alias.namespace.clone(),
            entry.alias.name.clone(),
        )
    });
    assert_eq!(canonically_ordered, all);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn revoked_grants_stop_covering_table_aliases(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let main = connection(&mut conn, acme, "main").await;
    let table = tables::resolve_table(&mut conn, acme, &alias(main, "events"), Uuid::now_v7())
        .await
        .unwrap()
        .id;
    let principal = user(&mut conn, acme, "reader").await;
    let grant = bergsmith_common::ids::RoleGrantId::generate();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: grant,
            organization_id: acme,
            principal_id: principal,
            role: Role::Operator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        tables::accessible_aliases_with_connections(
            &mut conn,
            acme,
            principal,
            &["Operator"],
            table
        )
        .await
        .unwrap()
        .len(),
        1
    );

    grants::revoke_grant(&mut conn, acme, grant, None)
        .await
        .unwrap();

    assert!(
        tables::accessible_aliases_with_connections(
            &mut conn,
            acme,
            principal,
            &["Operator"],
            table
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn deactivation_keeps_grants_stops_permission_and_leaves_work_alone(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    // Someone else must remain an administrator for the last-administrator
    // guard to allow the change under test.
    let carol = user(&mut conn, tenant.organization, "carol").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: carol,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();

    let (principal, changed) =
        tenancy::deactivate_principal(&mut conn, tenant.organization, tenant.user)
            .await
            .unwrap();
    assert!(changed);
    assert!(!principal.is_active());
    assert!(principal.deactivated_at.is_some());

    // The administrator grant is kept for audit but exercises nothing.
    let kept = grants::active_grants(&mut conn, tenant.organization, tenant.user)
        .await
        .unwrap();
    assert_eq!(kept.len(), 1);
    assert!(
        !grants::has_permission(
            &mut conn,
            tenant.organization,
            tenant.user,
            bergsmith_common::permission::Permission::ViewInventory,
            bergsmith_common::permission::ResourceScope::Organization,
        )
        .await
        .unwrap()
    );
    let job = bergsmith_persistence::jobs::job(&mut conn, tenant.organization, job)
        .await
        .unwrap();
    assert!(
        grants::require_job_permission(
            &mut conn,
            &job,
            bergsmith_common::permission::Permission::SubmitJobs,
        )
        .await
        .is_err()
    );
    // Running work is untouched: the job keeps its state and requester.
    assert_eq!(job.state, bergsmith_common::job::JobState::Planning);
    assert_eq!(job.created_by, tenant.user);

    // A repeated command changes nothing and reports no change.
    let (_, changed) = tenancy::deactivate_principal(&mut conn, tenant.organization, tenant.user)
        .await
        .unwrap();
    assert!(!changed);

    let (reactivated, changed) =
        tenancy::reactivate_principal(&mut conn, tenant.organization, tenant.user)
            .await
            .unwrap();
    assert!(changed);
    assert!(reactivated.is_active());
    assert!(
        grants::has_permission(
            &mut conn,
            tenant.organization,
            tenant.user,
            bergsmith_common::permission::Permission::ViewInventory,
            bergsmith_common::permission::ResourceScope::Organization,
        )
        .await
        .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn only_a_non_administrator_or_a_second_administrator_may_be_deactivated(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let refused = tenancy::deactivate_principal(&mut conn, tenant.organization, tenant.user).await;
    assert!(matches!(
        refused,
        Err(PersistenceError::LastAdministrator { .. })
    ));
    let stored = tenancy::principal(&mut conn, tenant.organization, tenant.user)
        .await
        .unwrap();
    assert!(stored.is_active());

    // A person without an administrator grant never blocks the change.
    let bob = user(&mut conn, tenant.organization, "bob").await;
    let (_, changed) = tenancy::deactivate_principal(&mut conn, tenant.organization, bob)
        .await
        .unwrap();
    assert!(changed);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_deactivations_leave_one_active_administrator(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let carol = user(&mut conn, tenant.organization, "carol").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: carol,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);

    // The first change holds the organization's lifecycle lock until it
    // commits, so a second change cannot count administrators behind its back.
    let mut tx = pool.begin().await.unwrap();
    tenancy::deactivate_principal(&mut tx, tenant.organization, tenant.user)
        .await
        .unwrap();
    let mut second = pool.acquire().await.unwrap();
    sqlx::query("SET statement_timeout = '250ms'")
        .execute(&mut *second)
        .await
        .unwrap();
    let blocked = tenancy::deactivate_principal(&mut second, tenant.organization, carol).await;
    assert!(
        matches!(blocked, Err(PersistenceError::Database(_))),
        "the second change must wait for the first: {blocked:?}"
    );
    sqlx::query("SET statement_timeout = 0")
        .execute(&mut *second)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let refused = tenancy::deactivate_principal(&mut second, tenant.organization, carol).await;
    assert!(matches!(
        refused,
        Err(PersistenceError::LastAdministrator { .. })
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn scoped_administrator_grants_do_not_replace_organization_administration(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let on_connection = user(&mut conn, tenant.organization, "bob").await;
    let on_table = user(&mut conn, tenant.organization, "carol").await;
    for (principal, scope) in [
        (on_connection, GrantScope::connection(tenant.connection)),
        (on_table, GrantScope::table(tenant.table)),
    ] {
        grants::create_grant(
            &mut conn,
            &NewRoleGrant {
                id: bergsmith_common::ids::RoleGrantId::generate(),
                organization_id: tenant.organization,
                principal_id: principal,
                role: Role::Administrator,
                scope,
                granted_by: None,
            },
        )
        .await
        .unwrap();
    }

    // Both replacements can administer their resource, but neither can manage
    // principals or restore organization administration.
    let refused = tenancy::deactivate_principal(&mut conn, tenant.organization, tenant.user).await;
    assert!(
        matches!(refused, Err(PersistenceError::LastAdministrator { .. })),
        "a scoped administrator must not replace the organization's last one: {refused:?}"
    );

    // A scoped-only administrator is not an organization administrator, so
    // deactivating them removes no organization administration.
    let (_, changed) = tenancy::deactivate_principal(&mut conn, tenant.organization, on_connection)
        .await
        .unwrap();
    assert!(changed);
}
