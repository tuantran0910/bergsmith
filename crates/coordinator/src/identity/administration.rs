//! Privileged administration: provisioning principals, and granting,
//! revoking, or terminating sessions.
//!
//! Each command first requires the actor to hold `grants:manage` over the
//! scope it changes, then writes the change and its audit record in the same
//! connection. A failed audit record fails the command, so a privileged change
//! is never applied without a record. These are the commands BS-018 exposes;
//! keeping them here means the audited path is the only path.

use std::path::{Path, PathBuf};

use bergsmith_common::ids::{OrganizationId, PrincipalId, RoleGrantId, WorkerPoolTokenId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::grants::{self, NewRoleGrant, RoleGrant};
use bergsmith_persistence::tenancy::{self, NewPrincipal, Principal};
use bergsmith_persistence::worker_tokens::{self, NewWorkerPoolToken, WorkerPoolToken};
use bergsmith_persistence::{PersistenceError, auth};
use bergsmith_protocol::identity::{ServiceToken, valid_pool};
use sqlx::{Connection, PgConnection};
use thiserror::Error;

use super::audit;
use super::authorization::Authorizer;
use super::random;

/// Why an administrative command failed.
#[derive(Debug, Error)]
pub enum AdministrationError {
    /// The actor holds no active grant that covers the command.
    #[error(transparent)]
    Authorization(#[from] super::authorization::AuthorizationError),
    /// The change conflicts with existing data or names a missing row.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The change could not be recorded, so it was not applied.
    #[error(transparent)]
    Audit(#[from] audit::AuditError),
    /// The operating system did not provide enough randomness for a token.
    #[error("a worker pool token could not be generated")]
    TokenGeneration,
}

impl AdministrationError {
    /// A stable reason code for operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Authorization(error) => error.reason_code(),
            Self::Persistence(_) => "AdministrationLedgerUnavailable",
            Self::Audit(_) => "AdministrationAuditUnavailable",
            Self::TokenGeneration => "WorkerTokenGenerationUnavailable",
        }
    }
}

impl From<sqlx::Error> for AdministrationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Persistence(PersistenceError::from(error))
    }
}

/// Provision a principal for a verified issuer and subject.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator, the
/// organization does not match, or the issuer and subject already exist.
pub async fn provision_principal(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    new: &NewPrincipal,
) -> Result<Principal, AdministrationError> {
    if new.organization_id != organization_id {
        return Err(PersistenceError::InvalidValue {
            entity: "principal",
            constraint: "provisioning_must_target_the_same_organization".to_owned(),
        }
        .into());
    }
    // Authorization, the mutation, and its audit row commit together; a
    // failed audit rolls the change back.
    let mut tx = conn.begin().await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let principal = tenancy::create_principal(&mut tx, new).await?;
    audit::principal_provisioned(
        &mut tx,
        organization_id,
        Some(actor),
        principal.id,
        &principal.issuer,
    )
    .await?;
    tx.commit().await?;
    Ok(principal)
}

/// Grant a role. The actor must be able to manage grants over the same scope.
///
/// The lifecycle lock fences authorization against revocation: a grant created
/// after the actor's own grant was revoked in an uncommitted transaction would
/// otherwise still see the committed grant, wait on the unique active-grant
/// index, and restore the access once that revocation commits. Holding the
/// lock through authorization, the insert, and the audit record makes the
/// revocation commit first, so the actor is denied against the state that
/// remains.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator over the
/// scope, or the grant already exists.
pub async fn grant_role(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    new: &NewRoleGrant,
) -> Result<RoleGrant, AdministrationError> {
    if new.organization_id != organization_id {
        return Err(PersistenceError::InvalidValue {
            entity: "role grant",
            constraint: "grant_must_target_the_same_organization".to_owned(),
        }
        .into());
    }
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        new.scope.resource(),
    )
    .await?;
    let created = grants::create_grant(&mut tx, new).await?;
    audit::grant_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::GrantCreated,
        created.principal_id,
        created.role.as_str(),
        Some(&created.id.to_string()),
    )
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Revoke a role grant. The actor must be able to manage grants over its scope.
///
/// The lifecycle lock serializes the last-administrator check with
/// deactivation, so two concurrent changes cannot both leave the organization
/// without an administrator.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator over the
/// scope, the grant is missing or already revoked, or revoking it would leave
/// the organization without an active administrator.
pub async fn revoke_role(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    grant_id: RoleGrantId,
) -> Result<RoleGrant, AdministrationError> {
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    let existing = grants::active_grant(&mut tx, organization_id, grant_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        existing.scope.resource(),
    )
    .await?;
    grants::ensure_administrator_replacement(&mut tx, organization_id, &existing).await?;
    let revoked = grants::revoke_grant(&mut tx, organization_id, grant_id, Some(actor)).await?;
    audit::grant_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::GrantRevoked,
        revoked.principal_id,
        revoked.role.as_str(),
        Some(&revoked.id.to_string()),
    )
    .await?;
    tx.commit().await?;
    Ok(revoked)
}

/// Deactivate a principal.
///
/// The row, its grants, and its history stay. Its sessions end in the same
/// transaction, sign-in refuses it, and the authorization check every
/// admission, publication, and command runs no longer applies its grants.
/// Running jobs are untouched.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator, the
/// principal is missing, or the change would leave the organization without
/// an active administrator.
pub async fn deactivate_principal(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
) -> Result<(Principal, u64), AdministrationError> {
    let mut tx = conn.begin().await?;
    // The lifecycle lock comes before authorization: deactivation and
    // reactivation both take it, so two changes to the same organization (for
    // example an administrator deactivating another while that one tries to
    // reactivate themselves) are authorized against the state the first change
    // leaves behind, not the state it is about to replace. The persistence
    // call acquires the same advisory lock again; PostgreSQL counts it and
    // releases it at commit.
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let (principal, changed) =
        tenancy::deactivate_principal(&mut tx, organization_id, principal_id).await?;
    let mut revoked = 0;
    if changed {
        // The audit row commits with the state change, so a deactivation is
        // never applied without a record.
        revoked =
            auth::revoke_principal_sessions(&mut tx, organization_id, principal_id, "deactivated")
                .await?;
        audit::principal_state_changed(
            &mut tx,
            organization_id,
            Some(actor),
            bergsmith_common::audit::AuditAction::PrincipalDeactivated,
            &principal,
            revoked,
        )
        .await?;
    }
    tx.commit().await?;
    Ok((principal, revoked))
}

/// Reactivate a principal. Its grants were never removed, so they apply again.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator, or the
/// principal is missing.
pub async fn reactivate_principal(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
) -> Result<Principal, AdministrationError> {
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let (principal, changed) =
        tenancy::reactivate_principal(&mut tx, organization_id, principal_id).await?;
    if changed {
        audit::principal_state_changed(
            &mut tx,
            organization_id,
            Some(actor),
            bergsmith_common::audit::AuditAction::PrincipalReactivated,
            &principal,
            0,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(principal)
}

/// Provision a service identity for a worker pool or a schedule.
///
/// A service identity has no external issuer or subject: it is created by an
/// administrator and can be granted roles like any other principal. Worker
/// pool tokens attach to it; a schedule names it as its execution identity.
///
/// # Errors
///
/// [`AdministrationError`] when the actor does not hold `workers:manage` over
/// the organization, or the ledger rejects the principal.
pub async fn create_service_identity(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    display_name: &str,
) -> Result<Principal, AdministrationError> {
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await?;
    let id = PrincipalId::generate();
    let principal = tenancy::create_principal(
        &mut tx,
        &NewPrincipal {
            id,
            organization_id,
            kind: tenancy::PrincipalKind::Service,
            issuer: "bergsmith:service".to_owned(),
            subject: id.to_string(),
            display_name: display_name.to_owned(),
        },
    )
    .await?;
    audit::principal_provisioned(
        &mut tx,
        organization_id,
        Some(actor),
        principal.id,
        &principal.issuer,
    )
    .await?;
    tx.commit().await?;
    Ok(principal)
}

/// A newly issued token and the one-time bearer value.
///
/// The bearer token is held in [`ServiceToken`], whose `Debug` redacts it.
/// Call [`Self::secret`] only at the response or file boundary.
#[derive(Clone, Debug)]
pub struct IssuedWorkerToken {
    /// Stored metadata; it never contains the bearer token.
    pub token: WorkerPoolToken,
    service_token: ServiceToken,
}

impl IssuedWorkerToken {
    /// The bearer token, for the single response or file that delivers it.
    #[must_use]
    pub fn secret(&self) -> &str {
        self.service_token.expose_secret()
    }
}

/// Why delivering a newly issued token to its operator failed.
#[derive(Debug, Error)]
pub enum TokenDeliveryError {
    /// The destination could not be reserved before issuance.
    #[error("token file {path} could not be created: {source}")]
    File {
        /// Destination that was not created.
        path: PathBuf,
        /// Operating-system failure.
        #[source]
        source: std::io::Error,
    },
    /// Issuance itself failed; no token exists.
    #[error(transparent)]
    Administration(#[from] AdministrationError),
    /// The token was issued but could not be written, so it was revoked.
    #[error("token {token} was issued but could not be written; it was revoked")]
    Revoked {
        /// The revoked token.
        token: WorkerPoolTokenId,
        /// Operating-system failure.
        #[source]
        source: std::io::Error,
    },
    /// The token was issued, could not be written, and could not be revoked.
    #[error("token {token} was issued but could not be written or revoked; revoke it manually")]
    Undelivered {
        /// The still-active token.
        token: WorkerPoolTokenId,
        /// The failed revocation.
        #[source]
        source: Box<AdministrationError>,
    },
    /// The token was revoked after a failed write, but its partial file could
    /// not be removed.
    #[error(
        "token {token} was issued but could not be written; it was revoked, but {path} could not be removed: {file_error}"
    )]
    RevokedResidue {
        /// The revoked token.
        token: WorkerPoolTokenId,
        /// File that may contain a partial token.
        path: PathBuf,
        /// The original write failure.
        #[source]
        write_error: std::io::Error,
        /// The failed removal.
        file_error: std::io::Error,
    },
    /// The token could not be written or revoked, and its partial file could
    /// not be removed. It may still authenticate.
    #[error(
        "token {token} was issued but could not be written or revoked; revoke it manually ({revocation}); {path} could not be removed: {file_error}"
    )]
    UndeliveredResidue {
        /// The token that may be active.
        token: WorkerPoolTokenId,
        /// File that may contain a partial token.
        path: PathBuf,
        /// The failed revocation.
        #[source]
        revocation: Box<AdministrationError>,
        /// The failed removal.
        file_error: std::io::Error,
    },
}

/// Issues a token and writes it to `destination` exactly once.
///
/// A failed write revokes the undelivered token, so a caller that reports an
/// error does not leave an active credential whose only copy was lost.
///
/// # Errors
///
/// [`TokenDeliveryError`] when issuance or writing fails.
pub async fn issue_worker_token_to_writer<W: std::io::Write>(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
    pool: &str,
    label: Option<&str>,
    destination: &mut W,
) -> Result<IssuedWorkerToken, TokenDeliveryError> {
    let issued =
        issue_worker_token(conn, organization_id, actor, principal_id, pool, label).await?;
    if let Err(source) = write_token(destination, issued.secret()) {
        let token = issued.token.id;
        return match revoke_undelivered(conn, organization_id, actor, principal_id, &issued.token)
            .await
        {
            Ok(()) => Err(TokenDeliveryError::Revoked { token, source }),
            Err(error) => Err(TokenDeliveryError::Undelivered {
                token,
                source: Box::new(error),
            }),
        };
    }
    Ok(issued)
}

/// Reserves a new mode-0600 file, issues a token, and writes it once.
///
/// The file is created before issuance, so an existing destination or an
/// unwritable directory fails without leaving an active token. If the write
/// fails after issuance, the token is revoked and the partial file is removed.
///
/// # Errors
///
/// [`TokenDeliveryError`] when the destination, issuance, writing, or cleanup
/// fails.
pub async fn issue_worker_token_to_file(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
    pool: &str,
    label: Option<&str>,
    path: &Path,
) -> Result<IssuedWorkerToken, TokenDeliveryError> {
    let mut file = reserve_token_file(path).map_err(|source| TokenDeliveryError::File {
        path: path.to_path_buf(),
        source,
    })?;
    match issue_worker_token_to_writer(
        conn,
        organization_id,
        actor,
        principal_id,
        pool,
        label,
        &mut file,
    )
    .await
    {
        Ok(issued) => Ok(issued),
        Err(error) => {
            drop(file);
            Err(finish_failed_delivery(
                error,
                path,
                std::fs::remove_file(path),
            ))
        }
    }
}

/// Combines a failed delivery with the result of removing its partial file.
///
/// A removal failure is only meaningful when a secret may have reached the
/// file. The original write or revocation error stays available so the caller
/// still learns whether the token was revoked.
fn finish_failed_delivery(
    error: TokenDeliveryError,
    path: &Path,
    removal: std::io::Result<()>,
) -> TokenDeliveryError {
    match (error, removal) {
        (
            TokenDeliveryError::Revoked {
                token,
                source: write_error,
            },
            Err(file_error),
        ) => TokenDeliveryError::RevokedResidue {
            token,
            path: path.to_path_buf(),
            write_error,
            file_error,
        },
        (
            TokenDeliveryError::Undelivered {
                token,
                source: revocation,
            },
            Err(file_error),
        ) => TokenDeliveryError::UndeliveredResidue {
            token,
            path: path.to_path_buf(),
            revocation,
            file_error,
        },
        // Issuance failed before a credential existed, so a leftover empty
        // reservation is not a secret-leak risk. A successful removal also
        // leaves the original error intact.
        (error, Ok(()) | Err(_)) => error,
    }
}

fn reserve_token_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_token<W: std::io::Write>(destination: &mut W, token: &str) -> std::io::Result<()> {
    writeln!(destination, "{token}")?;
    destination.flush()
}

/// Revokes a token whose bearer value was never delivered.
///
/// This is cleanup for a token this call just issued; it deliberately does not
/// re-check authorization or hold the lifecycle lock, because the secret is
/// already in memory and must not remain usable if delivery failed.
async fn revoke_undelivered(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
    token: &WorkerPoolToken,
) -> Result<(), AdministrationError> {
    let mut tx = conn.begin().await?;
    match worker_tokens::revoke(
        &mut tx,
        organization_id,
        principal_id,
        token.id,
        Some(actor),
    )
    .await
    {
        Ok(revoked) => {
            audit::worker_token_changed(
                &mut tx,
                organization_id,
                Some(actor),
                bergsmith_common::audit::AuditAction::WorkerTokenRevoked,
                &revoked,
                None,
            )
            .await?;
        }
        // Another caller already revoked it; no active credential remains.
        Err(PersistenceError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    tx.commit().await?;
    Ok(())
}

/// Issue a worker pool token for an active service identity.
///
/// The caller must hold `workers:manage` over the organization. Only the
/// verifier is stored; `secret` is the only copy of the bearer token.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not authorized, the principal is
/// not an active service identity, or the ledger rejects the token.
pub async fn issue_worker_token(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
    pool: &str,
    label: Option<&str>,
) -> Result<IssuedWorkerToken, AdministrationError> {
    if !valid_pool(pool) {
        return Err(PersistenceError::InvalidValue {
            entity: "worker pool token",
            constraint: "pool_must_be_lowercase_letters_digits_and_hyphens".to_owned(),
        }
        .into());
    }
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await?;
    worker_tokens::require_active_service(&mut tx, organization_id, principal_id).await?;
    let (service_token, verifier) = generate_token(pool)?;
    let token = worker_tokens::create(
        &mut tx,
        &NewWorkerPoolToken {
            id: WorkerPoolTokenId::generate(),
            organization_id,
            principal_id,
            pool: pool.to_owned(),
            label: label.map(str::to_owned),
            verifier,
            created_by: Some(actor),
            rotated_from: None,
        },
    )
    .await?;
    audit::worker_token_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::WorkerTokenIssued,
        &token,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(IssuedWorkerToken {
        token,
        service_token,
    })
}

/// Rotate a worker pool token: issue its replacement and revoke the old one in
/// one transaction. The replacement keeps the pool and label.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not authorized, the token is
/// missing or already revoked, or the ledger rejects the replacement.
pub async fn rotate_worker_token(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
    token_id: WorkerPoolTokenId,
) -> Result<IssuedWorkerToken, AdministrationError> {
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await?;
    worker_tokens::require_active_service(&mut tx, organization_id, principal_id).await?;
    let previous =
        worker_tokens::lock_active(&mut tx, organization_id, principal_id, token_id).await?;
    let (service_token, verifier) = generate_token(&previous.pool)?;
    let replacement = worker_tokens::create(
        &mut tx,
        &NewWorkerPoolToken {
            id: WorkerPoolTokenId::generate(),
            organization_id,
            principal_id,
            pool: previous.pool.clone(),
            label: previous.label.clone(),
            verifier,
            created_by: Some(actor),
            rotated_from: Some(previous.id),
        },
    )
    .await?;
    worker_tokens::revoke(
        &mut tx,
        organization_id,
        principal_id,
        previous.id,
        Some(actor),
    )
    .await?;
    audit::worker_token_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::WorkerTokenRotated,
        &replacement,
        Some(previous.id),
    )
    .await?;
    tx.commit().await?;
    Ok(IssuedWorkerToken {
        token: replacement,
        service_token,
    })
}

/// Revoke a worker pool token. Revocation is logical and effective on the next
/// worker RPC.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not authorized, or the token is
/// missing or already revoked.
pub async fn revoke_worker_token(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
    token_id: WorkerPoolTokenId,
) -> Result<WorkerPoolToken, AdministrationError> {
    let mut tx = conn.begin().await?;
    tenancy::lock_principal_administration(&mut tx, organization_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await?;
    worker_tokens::lock_active(&mut tx, organization_id, principal_id, token_id).await?;
    let revoked = worker_tokens::revoke(
        &mut tx,
        organization_id,
        principal_id,
        token_id,
        Some(actor),
    )
    .await?;
    audit::worker_token_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::WorkerTokenRevoked,
        &revoked,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(revoked)
}

/// Builds a fresh bearer token and its verifier.
fn generate_token(
    pool: &str,
) -> Result<(ServiceToken, bergsmith_common::digest::Sha256Digest), AdministrationError> {
    let secret = random::token().ok_or(AdministrationError::TokenGeneration)?;
    let raw = format!("bsw1_{pool}_{secret}");
    let token = ServiceToken::parse(&raw).map_err(|_| AdministrationError::TokenGeneration)?;
    let verifier = token.verifier();
    Ok((token, verifier))
}

/// Revoke every active session of a principal, for example after an incident.
///
/// The actor must be able to manage grants over the organization, because
/// terminating sessions removes access broadly.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator, and
/// persistence errors otherwise.
pub async fn revoke_sessions(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
) -> Result<u64, AdministrationError> {
    let mut tx = conn.begin().await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let revoked =
        auth::revoke_principal_sessions(&mut tx, organization_id, principal_id, "revoked").await?;
    if revoked > 0 {
        audit::sessions_revoked(&mut tx, organization_id, Some(actor), principal_id, revoked)
            .await?;
    }
    tx.commit().await?;
    Ok(revoked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_token_debug_redacts_the_bearer_secret() {
        let secret = "bsw1_native_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0";
        let issued = IssuedWorkerToken {
            token: WorkerPoolToken {
                id: WorkerPoolTokenId::generate(),
                organization_id: OrganizationId::generate(),
                principal_id: PrincipalId::generate(),
                pool: "native".to_owned(),
                label: Some("worker-1".to_owned()),
                created_by: None,
                created_at: chrono::Utc::now(),
                rotated_from: None,
                revoked_by: None,
                revoked_at: None,
            },
            service_token: ServiceToken::parse(secret).unwrap(),
        };

        let debug = format!("{issued:?}");
        assert!(!debug.contains(secret), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert_eq!(issued.secret(), secret);
    }

    #[test]
    fn a_failed_revocation_survives_a_failed_file_removal() {
        let token = WorkerPoolTokenId::generate();
        let combined = finish_failed_delivery(
            TokenDeliveryError::Undelivered {
                token,
                source: Box::new(AdministrationError::TokenGeneration),
            },
            Path::new("/tmp/worker-token"),
            Err(std::io::Error::other("removal failed")),
        );

        let TokenDeliveryError::UndeliveredResidue {
            token: reported,
            revocation,
            file_error,
            ..
        } = &combined
        else {
            panic!("expected an undelivered residue, got {combined:?}");
        };
        assert_eq!(*reported, token);
        assert!(matches!(
            revocation.as_ref(),
            AdministrationError::TokenGeneration
        ));
        assert_eq!(file_error.kind(), std::io::ErrorKind::Other);
        let message = combined.to_string();
        assert!(message.contains("revoke it manually"), "{message}");
        assert!(message.contains("could not be removed"), "{message}");
    }
}
