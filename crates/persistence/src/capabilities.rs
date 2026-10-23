//! Recorded capability checks for catalog connections (BS-022.T2).
//!
//! A capability check asks one catalog what it supports and records the
//! answer with the time of the check. The record is diagnostic: it never
//! changes a connection's configuration, so it does not move the
//! configuration revision and it is not an authorization to publish.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use bergsmith_common::capabilities::ConnectionCapabilities;
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId};

use crate::PersistenceError;

/// One recorded check and the time it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityCheck {
    /// What the catalog advertised.
    pub capabilities: ConnectionCapabilities,
    /// The fingerprint of the routing the check asked about. An answer whose
    /// fingerprint no longer matches the connection is stale.
    pub config_fingerprint: String,
    /// When the check ran (database time).
    pub checked_at: DateTime<Utc>,
}

/// Records the latest check of a connection's catalog.
///
/// The check replaces the previous one: a connection has one current answer,
/// and the audit log keeps the history of the decisions. The check time is
/// the database clock, so a recorded answer and its row agree. The
/// fingerprint binds the answer to the settings it describes.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when the connection does not exist
/// in the organization, and database errors.
pub async fn record_check(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    connection_id: CatalogConnectionId,
    capabilities: &ConnectionCapabilities,
    config_fingerprint: &str,
) -> Result<DateTime<Utc>, PersistenceError> {
    let checked_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO catalog_connection_checks
             (organization_id, catalog_connection_id, capabilities, config_fingerprint, checked_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (organization_id, catalog_connection_id)
         DO UPDATE SET capabilities = EXCLUDED.capabilities,
                       config_fingerprint = EXCLUDED.config_fingerprint,
                       checked_at = now()
         RETURNING checked_at",
    )
    .bind(organization_id.as_uuid())
    .bind(connection_id.as_uuid())
    .bind(
        serde_json::to_value(capabilities).map_err(|error| PersistenceError::InvalidValue {
            entity: "catalog capability record",
            constraint: error.to_string(),
        })?,
    )
    .bind(config_fingerprint)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("catalog capability record", error))?;
    Ok(checked_at)
}

/// Reads the latest recorded check of a connection's catalog.
///
/// # Errors
///
/// Database errors. A connection that was never checked has no row, and
/// neither does a connection whose check was removed.
pub async fn latest_check(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    connection_id: CatalogConnectionId,
) -> Result<Option<CapabilityCheck>, PersistenceError> {
    let row: Option<(serde_json::Value, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT capabilities, config_fingerprint, checked_at FROM catalog_connection_checks
         WHERE organization_id = $1 AND catalog_connection_id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(connection_id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((capabilities, config_fingerprint, checked_at)) = row else {
        return Ok(None);
    };
    let capabilities: ConnectionCapabilities =
        serde_json::from_value(capabilities).map_err(|error| PersistenceError::InvalidValue {
            entity: "catalog capability record",
            constraint: error.to_string(),
        })?;
    Ok(Some(CapabilityCheck {
        capabilities,
        config_fingerprint,
        checked_at,
    }))
}
