//! Saved list views (BS-033.T6, ADR 0067).
//!
//! A view belongs to one person in one organization; every read and write
//! is scoped by both, so another person's view is indistinguishable from a
//! missing one. Edits and deletes name the revision the caller read. The
//! per-person limit is checked under a transaction-scoped advisory lock on
//! the owner, so concurrent saves cannot pass it together.

use std::collections::BTreeMap;

use bergsmith_common::ids::{OrganizationId, PrincipalId, SavedViewId};
use bergsmith_common::views::{MAX_VIEWS_PER_PRINCIPAL, ViewPage, ViewParams};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::PersistenceError;

const ENTITY: &str = "saved view";

/// A stored view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedView {
    /// The view.
    pub id: SavedViewId,
    /// The list it belongs to.
    pub page: ViewPage,
    /// The name its owner gave it.
    pub name: String,
    /// The list's search parameters, by name.
    pub params: BTreeMap<String, String>,
    /// Increments on every edit.
    pub revision: i64,
    /// When it was saved.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A view to save.
#[derive(Clone, Debug)]
pub struct NewSavedView<'a> {
    /// The new view's identifier.
    pub id: SavedViewId,
    /// The organization it is saved in.
    pub organization_id: OrganizationId,
    /// Its owner.
    pub principal_id: PrincipalId,
    /// The list it belongs to.
    pub page: ViewPage,
    /// A name from [`bergsmith_common::views::view_name`].
    pub name: &'a str,
    /// Parameters checked against `page`.
    pub params: &'a ViewParams,
}

/// An edit of a view; absent fields keep their values.
#[derive(Clone, Debug)]
pub struct SavedViewUpdate<'a> {
    /// A new name from [`bergsmith_common::views::view_name`].
    pub name: Option<&'a str>,
    /// New parameters checked against the view's page.
    pub params: Option<&'a ViewParams>,
    /// The revision the caller read.
    pub expected_revision: i64,
}

#[derive(sqlx::FromRow)]
struct ViewRow {
    id: Uuid,
    page: String,
    name: String,
    params: Json<BTreeMap<String, String>>,
    revision: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ViewRow> for SavedView {
    type Error = PersistenceError;

    fn try_from(row: ViewRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: SavedViewId::from_uuid(row.id),
            page: row
                .page
                .parse()
                .map_err(|_| PersistenceError::InvalidValue {
                    entity: ENTITY,
                    constraint: "page".to_owned(),
                })?,
            name: row.name,
            params: row.params.0,
            revision: row.revision,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

macro_rules! view_columns {
    () => {
        "id, page, name, params, revision, created_at, updated_at"
    };
}

/// The owner's views, of one list when `page` is given, oldest first.
///
/// # Errors
///
/// A database failure, or a stored row this version cannot read.
pub async fn list(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    page: Option<ViewPage>,
) -> Result<Vec<SavedView>, PersistenceError> {
    let rows = sqlx::query_as::<_, ViewRow>(concat!(
        "SELECT ",
        view_columns!(),
        " FROM saved_views
         WHERE organization_id = $1 AND principal_id = $2
           AND ($3::text IS NULL OR page = $3)
         ORDER BY page, id"
    ))
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(page.map(ViewPage::as_str))
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(SavedView::try_from).collect()
}

/// One of the owner's views.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the owner has no such view, including
/// another person's view.
pub async fn get(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: SavedViewId,
) -> Result<SavedView, PersistenceError> {
    sqlx::query_as::<_, ViewRow>(concat!(
        "SELECT ",
        view_columns!(),
        " FROM saved_views
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3"
    ))
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: ENTITY,
        id: id.to_string(),
    })?
    .try_into()
}

/// Saves a view. Call inside a transaction: the limit check holds an
/// advisory lock on the owner until it ends.
///
/// # Errors
///
/// [`PersistenceError::LimitReached`] when the owner already keeps
/// [`MAX_VIEWS_PER_PRINCIPAL`] views, [`PersistenceError::AlreadyExists`] for
/// a name the owner already uses on the list, or a database failure.
pub async fn create(
    conn: &mut PgConnection,
    view: &NewSavedView<'_>,
) -> Result<SavedView, PersistenceError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('saved_views:' || $1::text, 0))")
        .bind(view.principal_id.as_uuid())
        .execute(&mut *conn)
        .await?;
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM saved_views WHERE organization_id = $1 AND principal_id = $2",
    )
    .bind(view.organization_id.as_uuid())
    .bind(view.principal_id.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    if kept >= MAX_VIEWS_PER_PRINCIPAL {
        return Err(PersistenceError::LimitReached {
            entity: ENTITY,
            limit: MAX_VIEWS_PER_PRINCIPAL,
        });
    }
    sqlx::query_as::<_, ViewRow>(concat!(
        "INSERT INTO saved_views (id, organization_id, principal_id, page, name, params)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING ",
        view_columns!()
    ))
    .bind(view.id.as_uuid())
    .bind(view.organization_id.as_uuid())
    .bind(view.principal_id.as_uuid())
    .bind(view.page.as_str())
    .bind(view.name)
    .bind(Json(view.params.as_map()))
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write(ENTITY, error))?
    .try_into()
}

/// Edits one of the owner's views when the revision still matches.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] for no such view,
/// [`PersistenceError::ConfigRevisionConflict`] when it moved on,
/// [`PersistenceError::AlreadyExists`] for a name already in use, or a
/// database failure.
pub async fn update(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: SavedViewId,
    update: &SavedViewUpdate<'_>,
) -> Result<SavedView, PersistenceError> {
    let row = sqlx::query_as::<_, ViewRow>(concat!(
        "UPDATE saved_views
         SET name = COALESCE($4, name),
             params = COALESCE($5, params),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3 AND revision = $6
         RETURNING ",
        view_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .bind(update.name)
    .bind(update.params.map(|params| Json(params.as_map())))
    .bind(update.expected_revision)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write(ENTITY, error))?;
    match row {
        Some(row) => row.try_into(),
        None => Err(missing_or_moved(
            conn,
            organization_id,
            principal_id,
            id,
            update.expected_revision,
        )
        .await?),
    }
}

/// Deletes one of the owner's views when the revision still matches.
///
/// # Errors
///
/// As for [`update`], without the name conflict.
pub async fn delete(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: SavedViewId,
    expected_revision: i64,
) -> Result<SavedView, PersistenceError> {
    let row = sqlx::query_as::<_, ViewRow>(concat!(
        "DELETE FROM saved_views
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3 AND revision = $4
         RETURNING ",
        view_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .bind(expected_revision)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(row) => row.try_into(),
        None => Err(
            missing_or_moved(conn, organization_id, principal_id, id, expected_revision).await?,
        ),
    }
}

/// Distinguishes "no such view" from "the view moved to another revision".
async fn missing_or_moved(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: SavedViewId,
    expected: i64,
) -> Result<PersistenceError, PersistenceError> {
    let actual: Option<i64> = sqlx::query_scalar(
        "SELECT revision FROM saved_views
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match actual {
        Some(actual) => PersistenceError::ConfigRevisionConflict {
            entity: ENTITY,
            id: id.to_string(),
            expected,
            actual,
        },
        None => PersistenceError::NotFound {
            entity: ENTITY,
            id: id.to_string(),
        },
    })
}
