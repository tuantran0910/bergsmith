//! Catalog discovery and table registration (BS-022.T4).
//!
//! Browsing reads a connection's catalog one page at a time; registering
//! records the catalog names an operator chooses, and unregistering removes
//! the exact name the operator saw. The catalog calls never hold a ledger
//! lock, and every write is authorized, audited, and idempotent.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bergsmith_catalog::{CatalogError, TableName};
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::permission::ResourceScope;
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::tables::{self, RegisteredAlias, TableAlias};
use bergsmith_persistence::tenancy::{self, CatalogConnection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::catalogs::{authorized_transaction, classify, fingerprint};
use super::{ApiError, ApiState, Caller, json_body};
use crate::discovery::DiscoveryError;
use crate::runtime::catalogs::AccessError;

/// The items one browse request asks the catalog for unless it asks for fewer.
const DEFAULT_BROWSE_LIMIT: usize = 50;

/// The most items one browse request asks the catalog for.
const MAX_BROWSE_LIMIT: usize = 200;

/// The most identifier bytes one namespace registration enumerates.
const MAX_REGISTRATION_BYTES: usize = 1024 * 1024;

/// The most levels one namespace may have.
const MAX_NAMESPACE_LEVELS: usize = 16;

/// The longest namespace level or table name, in bytes.
const MAX_NAME_LENGTH: usize = 255;

/// The version of the browse cursor this API writes.
const CURSOR_VERSION: u8 = 1;

/// The longest catalog token one cursor may carry.
const MAX_TOKEN_LENGTH: usize = 4096;

/// The longest cursor one request may carry.
///
/// A cursor holds three identifiers, the listing, a namespace digest, and a
/// token of up to [`MAX_TOKEN_LENGTH`], base64-encoded with room to spare.
const MAX_CURSOR_LENGTH: usize = 8192;

/// The domain of the namespace digest a cursor carries.
const NAMESPACE_DOMAIN: &[u8] = b"bergsmith.catalog.discovery.namespace";

/// A browse cursor: the catalog's own continuation token, bound to one
/// listing.
///
/// The token is opaque, so the cursor carries it unchanged. Everything else
/// binds the cursor to the caller and the listing it was issued for: a cursor
/// replayed against another connection, namespace, or principal reads nothing.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListingCursor {
    version: u8,
    organization: OrganizationId,
    principal: PrincipalId,
    connection: CatalogConnectionId,
    /// The listing this continues: `namespaces` or `tables`.
    listing: String,
    /// The digest of the namespace the listing runs under, so a long path
    /// never grows the cursor.
    namespace: String,
    /// The catalog's continuation token.
    token: String,
}

/// The names to register: one table, or a whole namespace.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegisterTables {
    /// Namespace levels, outermost first.
    namespace: Vec<String>,
    /// The one table to register. Absent registers every table in the
    /// namespace.
    table: Option<String>,
}

/// The parameters a browse or removal request carries.
struct Parameters {
    /// The namespace levels: the parent for a namespace browse, the namespace
    /// itself for a table browse, registration, or removal.
    levels: Vec<String>,
    /// The table a removal names.
    table: Option<String>,
    /// The items the request asks for.
    limit: usize,
    /// The continuation a browse carries.
    cursor: Option<String>,
    /// The alias revision a removal observed.
    revision: Option<i64>,
}

/// Lists one page of the namespaces under a parent, or the top level.
///
/// # Errors
///
/// Returns 400 for an unparsable query or cursor, 403 without
/// `connections:manage` for the connection, 404 for an unknown connection or
/// namespace, 409 when the catalog does not list namespaces or answers a page
/// too large to carry, and a transient error when the catalog is unreachable
/// or the coordinator is at its discovery capacity.
pub(super) async fn namespaces(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = connection_id(id)?;
    let parameters = parameters(query, "parent")?;
    if parameters.table.is_some() || parameters.revision.is_some() {
        return Err(ApiError::invalid());
    }
    let token = cursor(
        &parameters,
        caller.organization_id,
        caller.principal_id,
        id,
        "namespaces",
    )?;
    let connection = authorize(&state, &caller, id).await?;
    let listing = state
        .discovery
        .namespaces(
            &connection,
            &parameters.levels,
            token.as_deref(),
            parameters.limit,
        )
        .await
        .map_err(classification)?;
    let next = next_cursor(
        caller.organization_id,
        caller.principal_id,
        id,
        "namespaces",
        &parameters.levels,
        listing.next_page_token.as_deref(),
    )?;
    Ok(Json(json!({
        "items": listing.items.into_iter().map(|levels| {
            // The name is for display; the levels are the identifier.
            let name = levels.join(".");
            json!({"name": name, "levels": levels})
        }).collect::<Vec<_>>(),
        "next_cursor": next,
        "skipped": listing.skipped,
    })))
}

/// Lists one page of a namespace's tables, with the ones Bergsmith tracks.
///
/// # Errors
///
/// [`namespaces`]'s errors, and 400 when the request names no namespace.
pub(super) async fn tables(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = connection_id(id)?;
    let parameters = parameters(query, "namespace")?;
    if parameters.table.is_some() || parameters.revision.is_some() || parameters.levels.is_empty() {
        return Err(ApiError::invalid());
    }
    let token = cursor(
        &parameters,
        caller.organization_id,
        caller.principal_id,
        id,
        "tables",
    )?;
    let connection = authorize(&state, &caller, id).await?;
    let listing = state
        .discovery
        .tables(
            &connection,
            &parameters.levels,
            token.as_deref(),
            parameters.limit,
        )
        .await
        .map_err(classification)?;
    let names: Vec<String> = listing
        .items
        .iter()
        .map(|table| table.name.clone())
        .collect();
    let mut conn = state.database.pool().acquire().await?;
    let registered = tables::registered_aliases(
        &mut conn,
        caller.organization_id,
        id,
        &parameters.levels,
        &names,
    )
    .await?;
    let items: Vec<Value> = listing
        .items
        .into_iter()
        .map(|table| {
            let registered = registered.get(&table.name);
            json!({
                "namespace": table.namespace,
                "name": table.name,
                "registered": registered.is_some(),
                // The revision names the exact alias row, so a removal can
                // say which one it saw.
                "table_id": registered.map(|name| name.table_id),
                "revision": registered.map(|name| name.revision),
            })
        })
        .collect();
    let next = next_cursor(
        caller.organization_id,
        caller.principal_id,
        id,
        "tables",
        &parameters.levels,
        listing.next_page_token.as_deref(),
    )?;
    Ok(Json(json!({
        "items": items,
        "next_cursor": next,
        "skipped": listing.skipped,
    })))
}

/// Registers one table, or every table of a namespace.
///
/// The catalog is read before anything is written, and the aliases and their
/// audit record commit together: a request either records every table it names
/// or none. Registering a name that is registered already answers with the
/// identity the first request recorded.
///
/// # Errors
///
/// Returns 400 for an unusable name, 403 without `connections:manage` for the
/// connection, 404 for an unknown connection or catalog table, 409 when the
/// namespace holds more tables than one request may record, holds a name
/// Bergsmith cannot address, or changed while it was read, and a transient
/// error when the catalog is unreachable or the coordinator is at its
/// discovery capacity.
pub(super) async fn register(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<RegisterTables>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = connection_id(id)?;
    let body = json_body(body)?;
    // Authorization comes first, like every other catalog mutation: an
    // unauthorized caller learns nothing from the request's validity.
    let connection = authorize(&state, &caller, id).await?;
    if body.namespace.is_empty()
        || !valid_namespace(&body.namespace)
        || body
            .table
            .as_deref()
            .is_some_and(|table| !valid_name(table))
    {
        return Err(ApiError::invalid());
    }
    // The names belong to the routing they were read with: an edit that lands
    // while the catalog is being read describes a connection the recorded
    // aliases would no longer belong to.
    let probed = fingerprint(&connection);
    let bound = state.discovery.limits().max_registration_tables;
    let names: Vec<TableName> = if let Some(table) = &body.table {
        vec![TableName {
            namespace: body.namespace.clone(),
            name: table.clone(),
        }]
    } else {
        namespace_tables(&state, &connection, &body.namespace, bound).await?
    };
    let loaded = state
        .discovery
        .load(&connection, &names)
        .await
        .map_err(classification)?;
    let aliases: Vec<(TableAlias, Uuid)> = loaded
        .iter()
        .map(|(name, table_uuid)| {
            (
                TableAlias {
                    connection_id: connection.id,
                    namespace: name.namespace.clone(),
                    name: name.name.clone(),
                },
                *table_uuid,
            )
        })
        .collect();
    let mut tx = authorized_transaction(&state, &caller, ResourceScope::Connection(id)).await?;
    // The shared lock keeps an edit from landing between this comparison and
    // the write below.
    let current =
        tenancy::catalog_connection_for_share(&mut tx, caller.organization_id, id).await?;
    if fingerprint(&current) != probed {
        return Err(ApiError::conflict(
            "catalog_connection_changed",
            "run_registration_again",
        ));
    }
    let registered = tables::register_aliases_in(&mut tx, caller.organization_id, &aliases).await?;
    let created = registered.iter().filter(|alias| alias.created).count();
    // A request that recorded nothing changed nothing, so it writes no audit
    // record; the answer says so with `created: false`.
    if created > 0 {
        record_registration(&mut tx, &caller, id, &body, &registered).await?;
    }
    tx.commit().await?;
    let results: Vec<Value> = loaded
        .iter()
        .zip(registered)
        .map(|((name, _), alias)| {
            json!({
                "table_id": alias.identity.id,
                "namespace": name.namespace,
                "name": name.name,
                "created": alias.created,
            })
        })
        .collect();
    // A request that recorded nothing new repeats an earlier one.
    let status = if created == 0 {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(json!({"registered": results}))).into_response())
}

/// Reads the whole list of tables a namespace registration records.
///
/// The catalog is followed page by page, so a namespace that spans several
/// pages registers as one request. Every page is checked for names the API
/// cannot address, and the enumeration is bounded by pages, identifier bytes,
/// and the registration bound; anything beyond a bound refuses the request
/// rather than recording part of the namespace.
async fn namespace_tables(
    state: &ApiState,
    connection: &CatalogConnection,
    namespace: &[String],
    bound: usize,
) -> Result<Vec<TableName>, ApiError> {
    let mut names: Vec<TableName> = Vec::new();
    let mut bytes = 0usize;
    let mut token: Option<String> = None;
    // One page per table at most, plus the page that proves there are more.
    let pages = bound.saturating_add(1);
    for _ in 0..pages {
        let page = state
            .discovery
            .tables(
                connection,
                namespace,
                token.as_deref(),
                bound.saturating_add(1),
            )
            .await
            .map_err(classification)?;
        if page.skipped > 0 {
            // A name the API could not address would be silently left out of a
            // request that asked for the whole namespace.
            return Err(ApiError::conflict(
                "registration_incomplete",
                "register_tables_individually",
            ));
        }
        bytes += page
            .items
            .iter()
            .map(|table| {
                table
                    .namespace
                    .iter()
                    .map(|level| level.len() + 1)
                    .sum::<usize>()
                    + table.name.len()
            })
            .sum::<usize>();
        if bytes > MAX_REGISTRATION_BYTES {
            return Err(ApiError::conflict(
                "registration_too_large",
                "register_tables_individually",
            ));
        }
        names.extend(page.items);
        if names.len() > bound {
            return Err(ApiError::conflict(
                "registration_too_large",
                "register_tables_individually",
            ));
        }
        let Some(next) = page.next_page_token else {
            names.sort();
            names.dedup();
            return Ok(names);
        };
        token = Some(next);
    }
    // The catalog kept handing out pages without ending the namespace.
    Err(ApiError::conflict(
        "registration_too_large",
        "register_tables_individually",
    ))
}

/// Removes the alias revision the caller observed, leaving the table.
///
/// # Errors
///
/// Returns 400 for an unusable name or a missing revision, 403 without
/// `connections:manage` for the connection, 404 when the connection has no
/// such alias, 409 when the alias is not the row the caller observed or the
/// table still has nonterminal jobs.
pub(super) async fn unregister(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = connection_id(id)?;
    let parameters = parameters(query, "namespace")?;
    let mut tx = authorized_transaction(&state, &caller, ResourceScope::Connection(id)).await?;
    let (Some(name), Some(revision), false) = (
        parameters.table,
        parameters.revision,
        parameters.levels.is_empty(),
    ) else {
        return Err(ApiError::invalid());
    };
    if !valid_name(&name) {
        return Err(ApiError::invalid());
    }
    let alias = TableAlias {
        connection_id: id,
        namespace: parameters.levels,
        name,
    };
    let table_id =
        tables::unregister_alias(&mut tx, caller.organization_id, &alias, revision).await?;
    record_unregistration(&mut tx, &caller, id, &alias, table_id).await?;
    tx.commit().await?;
    Ok(Json(json!({"table_id": table_id})))
}

fn connection_id(id: Result<Path<Uuid>, PathRejection>) -> Result<CatalogConnectionId, ApiError> {
    Ok(CatalogConnectionId::from_uuid(
        id.map_err(|_| ApiError::invalid())?.0,
    ))
}

/// Reads the query parameters, rejecting anything this request does not take.
fn parameters(
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    namespace_key: &str,
) -> Result<Parameters, ApiError> {
    let Query(pairs) = query.map_err(|_| ApiError::invalid())?;
    let mut levels = Vec::new();
    let mut table = None;
    let mut limit = DEFAULT_BROWSE_LIMIT;
    let mut cursor = None;
    let mut revision = None;
    for (name, value) in pairs {
        match name.as_str() {
            // A repeated `table`, `cursor`, or `revision` is a different
            // request, not last-wins.
            "table" if table.is_none() => table = Some(value),
            "cursor" if cursor.is_none() => cursor = Some(value),
            "revision" if revision.is_none() => {
                revision = Some(value.parse().map_err(|_| ApiError::invalid())?);
            }
            "limit" => limit = value.parse().map_err(|_| ApiError::invalid())?,
            name if name == namespace_key => levels.push(value),
            _ => return Err(ApiError::invalid()),
        }
    }
    if !(1..=MAX_BROWSE_LIMIT).contains(&limit)
        || !valid_namespace(&levels)
        || cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > MAX_CURSOR_LENGTH)
        || revision.is_some_and(|revision| revision < 0)
    {
        return Err(ApiError::invalid());
    }
    Ok(Parameters {
        levels,
        table,
        limit,
        cursor,
        revision,
    })
}

/// The catalog token a browse continues from, bound to this caller and
/// listing.
fn cursor(
    parameters: &Parameters,
    organization: OrganizationId,
    principal: PrincipalId,
    connection: CatalogConnectionId,
    listing: &str,
) -> Result<Option<String>, ApiError> {
    let Some(raw) = parameters.cursor.as_deref() else {
        return Ok(None);
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| ApiError::invalid())?;
    let cursor: ListingCursor = serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid())?;
    if cursor.version != CURSOR_VERSION
        || cursor.organization != organization
        || cursor.principal != principal
        || cursor.connection != connection
        || cursor.listing != listing
        || cursor.namespace != namespace_fingerprint(&parameters.levels)
        || cursor.token.is_empty()
        || cursor.token.len() > MAX_TOKEN_LENGTH
    {
        return Err(ApiError::invalid());
    }
    Ok(Some(cursor.token))
}

/// The cursor that continues after `token`, or `None` when the catalog ended.
fn next_cursor(
    organization: OrganizationId,
    principal: PrincipalId,
    connection: CatalogConnectionId,
    listing: &str,
    namespace: &[String],
    token: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let Some(token) = token else {
        return Ok(None);
    };
    if token.len() > MAX_TOKEN_LENGTH {
        // A continuation this long cannot be carried in a cursor, and an
        // unusable one would fail the next request instead.
        return Err(ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_token_too_large",
            "contact_operator",
        ));
    }
    let cursor = ListingCursor {
        version: CURSOR_VERSION,
        organization,
        principal,
        connection,
        listing: listing.to_owned(),
        namespace: namespace_fingerprint(namespace),
        token: token.to_owned(),
    };
    let bytes = serde_json::to_vec(&cursor).map_err(|_| ApiError::internal())?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() > MAX_CURSOR_LENGTH {
        return Err(ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_token_too_large",
            "contact_operator",
        ));
    }
    Ok(Some(encoded))
}

/// The digest of one namespace, as a cursor carries it.
///
/// Levels are length-prefixed, so two different namespaces never hash alike.
fn namespace_fingerprint(levels: &[String]) -> String {
    let mut bytes = Vec::new();
    for level in levels {
        bytes.extend_from_slice(&u64::try_from(level.len()).unwrap_or(u64::MAX).to_be_bytes());
        bytes.extend_from_slice(level.as_bytes());
    }
    bergsmith_common::digest::Sha256Digest::of(NAMESPACE_DOMAIN, &bytes).to_hex()
}

/// A namespace is a bounded list of usable levels.
fn valid_namespace(levels: &[String]) -> bool {
    levels.len() <= MAX_NAMESPACE_LEVELS && levels.iter().all(|level| valid_name(level))
}

/// One namespace level or table name: nonempty, bounded, and printable.
///
/// A level never carries a control character, which also rules out the unit
/// separator a catalog uses to join levels into one path segment.
fn valid_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAME_LENGTH && !value.chars().any(char::is_control)
}

/// Reads the connection after authorizing its management.
///
/// The ledger connection is released before the catalog is contacted: the
/// authorization is repeated inside the write transaction, so a grant revoked
/// meanwhile still refuses the write.
async fn authorize(
    state: &ApiState,
    caller: &Caller,
    id: CatalogConnectionId,
) -> Result<CatalogConnection, ApiError> {
    let mut tx = authorized_transaction(state, caller, ResourceScope::Connection(id)).await?;
    let connection = tenancy::catalog_connection(&mut tx, caller.organization_id, id).await?;
    tx.commit().await?;
    Ok(connection)
}

/// Records the registration and what it created.
async fn record_registration(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    id: CatalogConnectionId,
    request: &RegisterTables,
    registered: &[RegisteredAlias],
) -> Result<(), ApiError> {
    let created = registered.iter().filter(|alias| alias.created).count();
    // A namespace registration names its namespace and counts, never every
    // table it recorded; the aliases themselves are the record of what exists.
    let detail = match request.table.as_deref() {
        Some(table) => json!({
            "namespace": request.namespace,
            "table": table,
            "table_id": registered.first().map(|alias| alias.identity.id),
            "created": created == 1,
        }),
        None => json!({
            "namespace": request.namespace,
            "requested": registered.len(),
            "created": created,
        }),
    };
    bergsmith_persistence::audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: ActorKind::User,
            action: AuditAction::CatalogTablesRegistered.as_str(),
            resource_type: "catalog",
            resource_id: Some(&id.to_string()),
            outcome: AuditOutcome::Allowed,
            detail,
        },
    )
    .await?;
    Ok(())
}

/// Records the removal of one alias.
async fn record_unregistration(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    id: CatalogConnectionId,
    alias: &TableAlias,
    table_id: TableId,
) -> Result<(), ApiError> {
    bergsmith_persistence::audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: ActorKind::User,
            action: AuditAction::CatalogTableUnregistered.as_str(),
            resource_type: "catalog",
            resource_id: Some(&id.to_string()),
            outcome: AuditOutcome::Allowed,
            detail: json!({
                "namespace": alias.namespace,
                "table": alias.name,
                "table_id": table_id,
            }),
        },
    )
    .await?;
    Ok(())
}

/// Classifies a discovery failure without logging a catalog answer.
///
/// The catalog mapping matches the capability-check and preview contracts, so
/// one catalog failure reads the same way wherever it happens.
fn classification(error: DiscoveryError) -> ApiError {
    match error {
        DiscoveryError::Overloaded => {
            ApiError::transient("catalog_discovery_capacity_exhausted", "retry_same_request")
        }
        DiscoveryError::Access(error) => access(&error),
        DiscoveryError::Catalog(CatalogError::Status { status: 404, .. }) => ApiError::permanent(
            StatusCode::NOT_FOUND,
            "catalog_namespace_not_found",
            "choose_another_namespace",
        ),
        DiscoveryError::Catalog(error) => classify(&error),
        DiscoveryError::Unsupported { .. } => ApiError::permanent(
            StatusCode::CONFLICT,
            "catalog_unsupported",
            "check_catalog_compatibility",
        ),
        DiscoveryError::NamespaceMismatch { .. } => ApiError::permanent(
            StatusCode::CONFLICT,
            "catalog_namespace_mismatch",
            "check_catalog_compatibility",
        ),
        DiscoveryError::Table {
            source: CatalogError::Status { status: 404, .. },
            ..
        } => ApiError::permanent(
            StatusCode::NOT_FOUND,
            "catalog_table_not_found",
            "choose_another_table",
        ),
        DiscoveryError::Table { source, .. } => classify(&source),
        DiscoveryError::TooManyTables { .. } => {
            ApiError::conflict("registration_too_large", "register_tables_individually")
        }
    }
}

/// Classifies a failure to use a stored connection.
///
/// Discovery only opens the catalog, so the other access failures mean the
/// runtime and this API disagree about what it can do.
fn access(error: &AccessError) -> ApiError {
    match error {
        AccessError::Catalog(error) => classify(error),
        AccessError::Persistence(_) => {
            tracing::warn!(error = %error, "catalog discovery ledger read failed");
            ApiError::unavailable()
        }
        AccessError::CredentialUnavailable(_) => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_credential_unavailable",
            "configure_catalog_credential",
        ),
        AccessError::InvalidConnection(_, _) => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_connection_invalid",
            "fix_catalog_connection",
        ),
        other => {
            tracing::error!(error = %other, "catalog discovery access failed");
            ApiError::internal()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(pairs: &[(&str, &str)], key: &str) -> Result<Parameters, ApiError> {
        let query = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        parameters(Ok(Query(query)), key)
    }

    fn binding() -> (OrganizationId, PrincipalId, CatalogConnectionId) {
        (
            OrganizationId::generate(),
            PrincipalId::generate(),
            CatalogConnectionId::generate(),
        )
    }

    fn encoded(
        organization: OrganizationId,
        principal: PrincipalId,
        connection: CatalogConnectionId,
        listing: &str,
        namespace: &[&str],
        token: &str,
    ) -> String {
        let levels: Vec<String> = namespace.iter().map(|level| (*level).to_owned()).collect();
        let bytes = serde_json::to_vec(&ListingCursor {
            version: CURSOR_VERSION,
            organization,
            principal,
            connection,
            listing: listing.to_owned(),
            namespace: namespace_fingerprint(&levels),
            token: token.to_owned(),
        })
        .unwrap();
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn params(cursor: Option<String>, namespace: &[&str]) -> Parameters {
        Parameters {
            levels: namespace.iter().map(|level| (*level).to_owned()).collect(),
            table: None,
            limit: DEFAULT_BROWSE_LIMIT,
            cursor,
            revision: None,
        }
    }

    #[test]
    fn repeated_levels_are_the_namespace_and_anything_else_is_refused() {
        let parsed = error(
            &[("namespace", "a"), ("namespace", "b"), ("limit", "10")],
            "namespace",
        )
        .unwrap();
        assert_eq!(parsed.levels, ["a".to_owned(), "b".to_owned()]);
        assert_eq!(parsed.limit, 10);

        for refused in [
            [("namespace", "a"), ("unknown", "b")].as_slice(),
            [("namespace", "a"), ("limit", "0")].as_slice(),
            [("namespace", "a"), ("limit", "201")].as_slice(),
            [("namespace", "a"), ("limit", "many")].as_slice(),
            [("namespace", "a"), ("table", "t"), ("table", "u")].as_slice(),
            [("namespace", "a"), ("cursor", "c"), ("cursor", "d")].as_slice(),
            [("namespace", "a"), ("revision", "-1")].as_slice(),
            [("namespace", "a"), ("revision", "one")].as_slice(),
            [("namespace", "")].as_slice(),
            [("namespace", "a\nb")].as_slice(),
        ] {
            assert!(error(refused, "namespace").is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_namespace_has_a_bounded_number_of_bounded_levels() {
        let deep: Vec<(&str, &str)> = (0..=MAX_NAMESPACE_LEVELS)
            .map(|_| ("namespace", "a"))
            .collect();
        assert!(error(&deep, "namespace").is_err());

        let long = "a".repeat(MAX_NAME_LENGTH + 1);
        assert!(error(&[("namespace", &long)], "namespace").is_err());
        assert!(error(&[("namespace", &"a".repeat(MAX_NAME_LENGTH))], "namespace").is_ok());
    }

    #[test]
    fn a_tampered_cursor_is_a_client_error() {
        let (organization, principal, connection) = binding();
        for damaged in ["not-base64".to_owned(), URL_SAFE_NO_PAD.encode(b"not json")] {
            let parameters = params(Some(damaged), &["db"]);
            assert!(cursor(&parameters, organization, principal, connection, "tables").is_err());
        }
    }

    #[test]
    fn a_cursor_never_reaches_another_caller_or_listing() {
        let (organization, principal, connection) = binding();
        let raw = encoded(organization, principal, connection, "tables", &["db"], "t2");
        let parameters = params(Some(raw), &["db"]);

        // The binding it was issued to reads it back.
        assert_eq!(
            cursor(&parameters, organization, principal, connection, "tables")
                .unwrap()
                .as_deref(),
            Some("t2")
        );

        // Another identity, connection, or listing is refused.
        let (other_organization, other_principal, other_connection) = binding();
        for (organization, principal, connection, listing) in [
            (other_organization, principal, connection, "tables"),
            (organization, other_principal, connection, "tables"),
            (organization, principal, other_connection, "tables"),
            (organization, principal, connection, "namespaces"),
        ] {
            assert!(cursor(&parameters, organization, principal, connection, listing).is_err());
        }
    }

    #[test]
    fn a_cursor_is_always_usable() {
        let (organization, principal, connection) = binding();
        let token = "t".repeat(MAX_TOKEN_LENGTH);
        let deep: Vec<String> = (0..MAX_NAMESPACE_LEVELS)
            .map(|_| "a".repeat(MAX_NAME_LENGTH))
            .collect();
        let levels: Vec<&str> = deep.iter().map(String::as_str).collect();

        let raw = next_cursor(
            organization,
            principal,
            connection,
            "tables",
            &deep,
            Some(&token),
        )
        .unwrap()
        .unwrap();
        assert!(raw.len() <= MAX_CURSOR_LENGTH, "{} bytes", raw.len());

        // What it emits, it accepts, whatever the namespace length.
        let parameters = params(Some(raw), &levels);
        assert_eq!(
            cursor(&parameters, organization, principal, connection, "tables")
                .unwrap()
                .as_deref(),
            Some(token.as_str())
        );

        // A token it could not carry is refused instead of returned unusable.
        let too_long = "t".repeat(MAX_TOKEN_LENGTH + 1);
        let error = next_cursor(
            organization,
            principal,
            connection,
            "tables",
            &["db".to_owned()],
            Some(&too_long),
        )
        .unwrap_err();
        assert_eq!(error.reason, "catalog_token_too_large");
    }

    #[test]
    fn a_cursor_bound_to_the_wrong_namespace_is_refused() {
        let (organization, principal, connection) = binding();
        let raw = encoded(
            organization,
            principal,
            connection,
            "tables",
            &["other"],
            "t2",
        );
        let parameters = params(Some(raw), &["db"]);
        assert!(cursor(&parameters, organization, principal, connection, "tables").is_err());
    }
}
