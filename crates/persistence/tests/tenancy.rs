//! Organizations, principals, catalog connections, and table identities
//! against real PostgreSQL (BS-003.T2).

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
        tenancy::update_catalog_connection(&mut editing, &new, 0, true).await
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
