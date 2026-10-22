//! Operator commands run by the coordinator binary (BS-020.T7): role grants
//! and attestation of unknown publication outcomes.
//!
//! Each command runs with the database URL, like `bootstrap-admin`, and
//! names an acting principal. Database access is the authentication; the
//! actor attributes the change and must hold `grants:manage` over what it
//! changes, so a principal that is not an administrator is never recorded as
//! one. Every change goes through the audited library paths.

use bergsmith_catalog::CatalogConnection;
use bergsmith_common::ids::{JobId, PrincipalId, RoleGrantId};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{GrantScope, Permission, ResourceScope, Role};
use bergsmith_persistence::grants::{self, NewRoleGrant, RoleGrant};
use bergsmith_persistence::jobs::{self, Job};
use bergsmith_persistence::tenancy::{self, NewPrincipal, Principal, PrincipalKind};
use bergsmith_persistence::{PersistenceError, leadership};
/// Re-exported so the binary can pass a connection without depending on `sqlx`.
pub use sqlx::PgConnection;
use thiserror::Error;

use crate::durable_publication::{self, DurablePublicationError};
use crate::identity::administration::{self, AdministrationError};
use crate::identity::authorization::{AuthorizationError, Authorizer};
use crate::runtime::catalogs::{AccessError, connection_config};

/// The longest attestation reason accepted.
pub const MAX_ATTESTATION_REASON_CHARS: usize = 500;

/// Why an operator command failed.
#[derive(Debug, Error)]
pub enum OperatorError {
    /// The ledger read or write failed, or names a missing row.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The audited administration path refused or failed the change.
    #[error(transparent)]
    Administration(#[from] AdministrationError),
    /// The actor holds no grant that covers the command.
    #[error(transparent)]
    Authorization(#[from] AuthorizationError),
    /// The stored catalog connection cannot be used.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// The catalog could not be reached to recheck evidence.
    #[error(transparent)]
    Catalog(#[from] bergsmith_catalog::CatalogError),
    /// Attestation refused the change; nothing was written.
    #[error(transparent)]
    Publication(#[from] DurablePublicationError),
    /// Service roles are granted with `bootstrap-service`.
    #[error("role {0} belongs to service principals; use bootstrap-service")]
    ServiceRole(Role),
    /// The attestation reason is empty or too long.
    #[error("the attestation reason must have 1 to {MAX_ATTESTATION_REASON_CHARS} characters")]
    InvalidReason,
    /// The job is not the unknown outcome the operator inspected.
    #[error(
        "job {job} is {state} at revision {revision}, not CommitOutcomeUnknown at revision {inspected}"
    )]
    NotInspectedOutcome {
        /// The job.
        job: JobId,
        /// Its current state.
        state: JobState,
        /// Its current revision.
        revision: i64,
        /// The revision the operator inspected.
        inspected: i64,
    },
    /// No coordinator holds a live lease, so the write cannot be fenced.
    #[error("no coordinator holds a live leadership lease; start one before attesting")]
    NoLiveLeader,
    /// The grantee already exists as a service principal, which holds only
    /// the service role.
    #[error("principal {0} is a service principal; use bootstrap-service")]
    ServicePrincipal(PrincipalId),
}

/// A role to grant to a person, identified by issuer and subject.
#[derive(Clone, Debug)]
pub struct RoleAssignment {
    /// Organization slug.
    pub organization: String,
    /// The administrator making the change.
    pub actor: PrincipalId,
    /// OIDC issuer of the grantee.
    pub issuer: String,
    /// OIDC subject of the grantee within that issuer.
    pub subject: String,
    /// Display name used if the grantee is provisioned now.
    pub display_name: String,
    /// The role to grant: Viewer, Operator, or Administrator.
    pub role: Role,
    /// What the grant covers.
    pub scope: GrantScope,
}

/// Grants a role, provisioning the grantee's principal first when it is new.
///
/// Provisioning and the grant are separate audited changes, so a refused
/// grant can leave a provisioned principal without a role; running the
/// command again reuses it. Provisioning requires `grants:manage` over the
/// organization, the grant requires it over `scope`.
///
/// # Errors
///
/// [`OperatorError::ServiceRole`] for `MaintenanceService`,
/// [`OperatorError::ServicePrincipal`] when the issuer and subject name a
/// service principal, and [`OperatorError`] when the organization is unknown, the actor is not an
/// administrator over the scope, or the same active grant exists.
pub async fn grant_role(
    conn: &mut PgConnection,
    request: &RoleAssignment,
) -> Result<RoleGrant, OperatorError> {
    if request.role == Role::MaintenanceService {
        return Err(OperatorError::ServiceRole(request.role));
    }
    let organization = tenancy::organization_by_slug(conn, &request.organization).await?;
    let principal = match tenancy::principal_by_subject(
        conn,
        organization.id,
        &request.issuer,
        &request.subject,
    )
    .await
    {
        Ok(principal) if principal.kind == PrincipalKind::Service => {
            return Err(OperatorError::ServicePrincipal(principal.id));
        }
        Ok(principal) => principal,
        Err(PersistenceError::NotFound { .. }) => {
            administration::provision_principal(
                conn,
                organization.id,
                request.actor,
                &NewPrincipal {
                    id: PrincipalId::generate(),
                    organization_id: organization.id,
                    kind: PrincipalKind::User,
                    issuer: request.issuer.clone(),
                    subject: request.subject.clone(),
                    display_name: request.display_name.clone(),
                },
            )
            .await?
        }
        Err(error) => return Err(error.into()),
    };
    Ok(administration::grant_role(
        conn,
        organization.id,
        request.actor,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization.id,
            principal_id: principal.id,
            role: request.role,
            scope: request.scope,
            granted_by: Some(request.actor),
        },
    )
    .await?)
}

/// Revokes an active grant.
///
/// # Errors
///
/// [`OperatorError`] when the organization or active grant is unknown, or
/// the actor is not an administrator over the grant's scope.
pub async fn revoke_role(
    conn: &mut PgConnection,
    organization: &str,
    actor: PrincipalId,
    grant: RoleGrantId,
) -> Result<RoleGrant, OperatorError> {
    let organization = tenancy::organization_by_slug(conn, organization).await?;
    Ok(administration::revoke_role(conn, organization.id, actor, grant).await?)
}

/// An active grant and the principal that holds it.
#[derive(Clone, Debug)]
pub struct GrantListing {
    /// The grant.
    pub grant: RoleGrant,
    /// Its holder.
    pub principal: Principal,
}

/// Lists the organization's active grants, by principal, role, and scope.
///
/// The actor must hold `grants:manage` over the organization, because the
/// listing names every principal and what it may do.
///
/// # Errors
///
/// [`OperatorError::Authorization`] when the actor is not an organization
/// administrator, and [`OperatorError::Persistence`] for an unknown
/// organization or an unavailable ledger.
pub async fn list_grants(
    conn: &mut PgConnection,
    organization: &str,
    actor: PrincipalId,
) -> Result<Vec<GrantListing>, OperatorError> {
    let organization = tenancy::organization_by_slug(conn, organization).await?;
    Authorizer::require(
        conn,
        organization.id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let mut listings = Vec::new();
    for grant in grants::organization_grants(conn, organization.id).await? {
        let principal = tenancy::principal(conn, organization.id, grant.principal_id).await?;
        listings.push(GrantListing { grant, principal });
    }
    Ok(listings)
}

/// An operator's attestation that an unknown outcome did not publish.
#[derive(Clone, Debug)]
pub struct NonPublicationAttestation {
    /// Organization slug.
    pub organization: String,
    /// The administrator attesting.
    pub actor: PrincipalId,
    /// The `CommitOutcomeUnknown` job.
    pub job: JobId,
    /// The job revision the operator inspected.
    pub revision: i64,
    /// Where the evidence is, such as a catalog log reference or ticket.
    pub reason: String,
}

/// Resolves a `CommitOutcomeUnknown` job by operator attestation.
///
/// The actor must hold `grants:manage` over the job's table. The command
/// borrows the live coordinator's epoch, so the write is fenced like the
/// scheduler's and refused if leadership changes; the job revision
/// serializes it against a concurrent reconciliation. Retained snapshot
/// evidence is rechecked first and a match records `Succeeded`, whatever was
/// attested. Nothing is ever resent. The `job.non_publication_attested`
/// audit record, with the reason, commits in the same transaction as the
/// resolution, so a failed audit write leaves the job unchanged.
///
/// # Errors
///
/// [`OperatorError::InvalidReason`], [`OperatorError::NotInspectedOutcome`],
/// and [`OperatorError::NoLiveLeader`] before any catalog call; catalog,
/// revision, leadership, and audit failures leave the job unchanged.
pub async fn attest_non_publication(
    conn: &mut PgConnection,
    request: &NonPublicationAttestation,
) -> Result<Job, OperatorError> {
    let reason = request.reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_ATTESTATION_REASON_CHARS {
        return Err(OperatorError::InvalidReason);
    }
    let organization = tenancy::organization_by_slug(conn, &request.organization).await?;
    let job = jobs::job(conn, organization.id, request.job).await?;
    Authorizer::require(
        conn,
        organization.id,
        request.actor,
        Permission::ManageGrants,
        ResourceScope::Table(job.table_id),
    )
    .await?;
    if job.state != JobState::CommitOutcomeUnknown || job.state_revision != request.revision {
        return Err(OperatorError::NotInspectedOutcome {
            job: job.id,
            state: job.state,
            revision: job.state_revision,
            inspected: request.revision,
        });
    }
    let Some(epoch) = leadership::live_epoch(conn).await? else {
        return Err(OperatorError::NoLiveLeader);
    };
    let connection_id = durable_publication::publication_connection(conn, job.id).await?;
    let connection = tenancy::catalog_connection(conn, organization.id, connection_id).await?;
    let catalog = CatalogConnection::connect(&connection_config(&connection)?).await?;
    Ok(durable_publication::attest_non_publication(
        conn,
        &job,
        epoch,
        &catalog,
        request.actor,
        reason,
    )
    .await?)
}
