//! Coordinator leadership: a lease decided by database time and a durable
//! epoch that grows with every change of holder.
//!
//! Leadership stops new grants and publication when lost; it cannot revoke a
//! commit request that was already authorized.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::PersistenceError;

/// The current leadership lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leadership {
    /// The holder's identifier. Use a fresh value per coordinator process.
    pub holder: String,
    /// Strictly increasing across holders.
    pub epoch: i64,
    /// When this holder acquired leadership (database time).
    pub acquired_at: DateTime<Utc>,
    /// When the lease lapses unless renewed (database time).
    pub lease_expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct LeadershipRow {
    holder: String,
    epoch: i64,
    acquired_at: DateTime<Utc>,
    lease_expires_at: DateTime<Utc>,
}

impl From<LeadershipRow> for Leadership {
    fn from(row: LeadershipRow) -> Self {
        Self {
            holder: row.holder,
            epoch: row.epoch,
            acquired_at: row.acquired_at,
            lease_expires_at: row.lease_expires_at,
        }
    }
}

fn millis(lease: Duration) -> i64 {
    i64::try_from(lease.as_millis()).unwrap_or(i64::MAX)
}

/// Takes leadership if nobody holds a live lease, with the next epoch.
///
/// Returns `None` while another lease is live, even one held under the same
/// `holder`: a restarted process must not inherit its predecessor's epoch, so
/// give every process a fresh holder identifier.
///
/// # Errors
///
/// [`PersistenceError::InvalidValue`] for an empty or overlong holder, and
/// [`PersistenceError::Database`] when the query fails.
pub async fn acquire_leadership(
    conn: &mut PgConnection,
    holder: &str,
    lease: Duration,
) -> Result<Option<Leadership>, PersistenceError> {
    let row = sqlx::query_as::<_, LeadershipRow>(
        "INSERT INTO coordinator_leadership (holder, epoch, acquired_at, lease_expires_at)
         VALUES ($1, 1, now(), now() + $2 * interval '1 millisecond')
         ON CONFLICT (singleton) DO UPDATE
             SET holder = EXCLUDED.holder,
                 epoch = coordinator_leadership.epoch + 1,
                 acquired_at = EXCLUDED.acquired_at,
                 lease_expires_at = EXCLUDED.lease_expires_at
             WHERE coordinator_leadership.lease_expires_at <= now()
         RETURNING holder, epoch, acquired_at, lease_expires_at",
    )
    .bind(holder)
    .bind(millis(lease))
    .fetch_optional(conn)
    .await
    .map_err(|error| PersistenceError::from_write("coordinator leadership", error))?;
    Ok(row.map(Leadership::from))
}

/// Extends the lease of the current holder.
///
/// # Errors
///
/// [`PersistenceError::LeadershipLost`] when `holder` and `epoch` no longer
/// hold a live lease; the caller must stop granting and publishing.
pub async fn renew_leadership(
    conn: &mut PgConnection,
    holder: &str,
    epoch: i64,
    lease: Duration,
) -> Result<Leadership, PersistenceError> {
    sqlx::query_as::<_, LeadershipRow>(
        "UPDATE coordinator_leadership
         SET lease_expires_at = now() + $3 * interval '1 millisecond'
         WHERE holder = $1 AND epoch = $2 AND lease_expires_at > now()
         RETURNING holder, epoch, acquired_at, lease_expires_at",
    )
    .bind(holder)
    .bind(epoch)
    .bind(millis(lease))
    .fetch_optional(conn)
    .await?
    .map(Leadership::from)
    .ok_or(PersistenceError::LeadershipLost { epoch })
}

/// Gives up leadership so another coordinator can take it without waiting
/// for the lease to lapse. Releasing a lease that is already lost does nothing.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn release_leadership(
    conn: &mut PgConnection,
    holder: &str,
    epoch: i64,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "UPDATE coordinator_leadership SET lease_expires_at = greatest(acquired_at, now())
         WHERE holder = $1 AND epoch = $2 AND lease_expires_at > now()",
    )
    .bind(holder)
    .bind(epoch)
    .execute(conn)
    .await?;
    Ok(())
}

/// Checks, inside the caller's transaction, that `epoch` still holds a live
/// lease after the row lock is acquired, and holds a share lock so leadership
/// cannot change before commit. Callers lock their job before this row and
/// recheck the lease if further blocking work precedes their durable decision.
pub(crate) async fn ensure_leadership(
    conn: &mut PgConnection,
    epoch: i64,
) -> Result<(), PersistenceError> {
    let deadline: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT lease_expires_at FROM coordinator_leadership WHERE epoch = $1 FOR SHARE",
    )
    .bind(epoch)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(deadline) = deadline else {
        return Err(PersistenceError::LeadershipLost { epoch });
    };
    let live: bool = sqlx::query_scalar("SELECT $1::timestamptz > clock_timestamp()")
        .bind(deadline)
        .fetch_one(conn)
        .await?;
    if live {
        Ok(())
    } else {
        Err(PersistenceError::LeadershipLost { epoch })
    }
}
