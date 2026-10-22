//! `bergsmith-coordinator` entry point. Startup wiring only; behavior lives in the
//! `bergsmith-coordinator` library crate.

use std::path::PathBuf;
use std::process::ExitCode;

use bergsmith_common::ids::{CatalogConnectionId, JobId, PrincipalId, RoleGrantId, TableId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::database::{self, BootstrapAdministrator, DatabaseUrl};
use bergsmith_coordinator::operator::{
    self, NonPublicationAttestation, OperatorError, RoleAssignment,
};
use bergsmith_coordinator::runtime::CoordinatorConfig;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "bergsmith-coordinator",
    version,
    about = "Bergsmith coordinator"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply pending database migrations, then exit.
    Migrate {
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Serve the browser API, worker RPC endpoint, and maintenance runtime.
    ///
    /// The configuration file holds non-secret settings; the database URL and
    /// listener addresses may be overridden by environment or flags.
    Serve {
        /// Coordinator JSON configuration.
        #[arg(long)]
        config: PathBuf,
        /// PostgreSQL connection URL, overriding the file.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: Option<String>,
        /// Browser listener address, overriding the file.
        #[arg(long, env = "BERGSMITH_HTTP_BIND")]
        http_bind: Option<String>,
        /// Worker RPC listener address, overriding the file.
        #[arg(long, env = "BERGSMITH_WORKER_BIND")]
        worker_bind: Option<String>,
    },
    /// Create an organization.
    CreateOrganization {
        /// URL-safe organization slug.
        #[arg(long)]
        slug: String,
        /// Human-readable name.
        #[arg(long)]
        display_name: String,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Add an Iceberg REST catalog connection.
    AddCatalogConnection {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Unique connection name.
        #[arg(long)]
        name: String,
        /// Catalog base URI.
        #[arg(long)]
        uri: String,
        /// Warehouse to request, if the catalog needs one.
        #[arg(long)]
        warehouse: Option<String>,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Register a table alias by loading it from its catalog.
    RegisterTable {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Catalog connection identifier.
        #[arg(long)]
        connection: CatalogConnectionId,
        /// Namespace levels, outermost first. Repeat for nested namespaces.
        #[arg(long = "namespace", required = true)]
        namespace: Vec<String>,
        /// Table name.
        #[arg(long)]
        table: String,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Provision a service principal for a worker pool or schedule.
    BootstrapService {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Human-readable name.
        #[arg(long)]
        display_name: String,
        /// Grant `MaintenanceService` on this canonical table, for a schedule.
        #[arg(long)]
        table: Option<TableId>,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Print the stored verifier for a worker pool token file.
    ServiceTokenVerifier {
        /// File containing one `bsw1` token.
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Provision the first administrator of an organization.
    ///
    /// Sign-in never provisions principals or grants, so run this once per
    /// organization before its users can authenticate. It is idempotent.
    BootstrapAdmin {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// OIDC issuer the administrator signs in through.
        #[arg(long)]
        issuer: String,
        /// OIDC subject within that issuer.
        #[arg(long)]
        subject: String,
        /// Human-readable display name.
        #[arg(long, default_value = "Administrator")]
        display_name: String,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Grant a person a role, provisioning their principal if it is new.
    ///
    /// The actor must hold `grants:manage` over the scope (and over the
    /// organization to provision). Omit `--connection` and `--table` for an
    /// organization-wide grant.
    GrantRole {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Principal ID of the administrator making the change.
        #[arg(long)]
        actor: PrincipalId,
        /// OIDC issuer the grantee signs in through.
        #[arg(long)]
        issuer: String,
        /// OIDC subject within that issuer.
        #[arg(long)]
        subject: String,
        /// Display name, used only when the principal is provisioned now.
        #[arg(long, default_value = "User")]
        display_name: String,
        /// `viewer`, `operator`, or `administrator`.
        #[arg(long, value_parser = parse_role)]
        role: Role,
        /// Limit the grant to one catalog connection.
        #[arg(long, conflicts_with = "table")]
        connection: Option<CatalogConnectionId>,
        /// Limit the grant to one canonical table.
        #[arg(long)]
        table: Option<TableId>,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Revoke an active role grant.
    RevokeRole {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Principal ID of the administrator making the change.
        #[arg(long)]
        actor: PrincipalId,
        /// The grant to revoke, as printed by `grant-role` or `list-grants`.
        #[arg(long)]
        grant: RoleGrantId,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Print the organization's active role grants.
    ListGrants {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Principal ID of an organization administrator.
        #[arg(long)]
        actor: PrincipalId,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Resolve a `CommitOutcomeUnknown` job after proving it did not publish.
    ///
    /// Run only after the checks in the recovery runbook. Retained snapshot
    /// evidence is rechecked first and wins. A coordinator must be running,
    /// and the actor must hold `grants:manage` over the job's table.
    AttestNonPublication {
        /// Organization slug.
        #[arg(long)]
        organization: String,
        /// Principal ID of the administrator attesting.
        #[arg(long)]
        actor: PrincipalId,
        /// The job to resolve.
        #[arg(long)]
        job: JobId,
        /// The job revision you inspected; a changed job is refused.
        #[arg(long)]
        revision: i64,
        /// Where the evidence is, such as a catalog log reference or ticket.
        #[arg(long)]
        reason: String,
        /// PostgreSQL connection URL.
        #[arg(long, env = "BERGSMITH_DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
}

/// Installs the log subscriber. `BERGSMITH_LOG_FORMAT` selects `text` (the
/// default) or `json`, whose lines include the active span and its parents;
/// `RUST_LOG` filters both. Returns an unrecognized format after installing
/// the text format, so the caller can report it.
fn init_logging() -> Result<(), String> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match std::env::var("BERGSMITH_LOG_FORMAT").as_deref() {
        Err(_) | Ok("" | "text") => tracing_subscriber::fmt().with_env_filter(filter).init(),
        Ok("json") => tracing_subscriber::fmt()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_env_filter(filter)
            .init(),
        Ok(other) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            return Err(other.to_owned());
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one match arm per administration command keeps the CLI exhaustive"
)]
#[tokio::main]
async fn main() -> ExitCode {
    if let Err(format) = init_logging() {
        tracing::error!(format, "BERGSMITH_LOG_FORMAT must be text or json");
        return ExitCode::FAILURE;
    }

    match Cli::parse().command {
        Command::Migrate { database_url } => {
            match database::migrate(DatabaseUrl::new(database_url)).await {
                Ok(()) => {
                    tracing::info!("database migrations are up to date");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    tracing::error!(error = %error, cause = ?std::error::Error::source(&error), "database migration failed");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Serve {
            config,
            database_url,
            http_bind,
            worker_bind,
        } => match load_config(&config, database_url, http_bind, worker_bind) {
            Ok(config) => match bergsmith_coordinator::runtime::serve(config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(error = %error, "coordinator runtime failed");
                    ExitCode::FAILURE
                }
            },
            Err(error) => {
                tracing::error!(error = %error, "coordinator configuration failed");
                ExitCode::FAILURE
            }
        },
        Command::CreateOrganization {
            slug,
            display_name,
            database_url,
        } => match database::create_organization(
            DatabaseUrl::new(database_url),
            &slug,
            &display_name,
        )
        .await
        {
            Ok(organization) => {
                tracing::info!(organization_id = %organization.id, slug = %organization.slug, "organization created");
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(error = %error, "organization creation failed");
                ExitCode::FAILURE
            }
        },
        Command::AddCatalogConnection {
            organization,
            name,
            uri,
            warehouse,
            database_url,
        } => match database::add_catalog_connection(
            DatabaseUrl::new(database_url),
            &organization,
            &name,
            &uri,
            warehouse,
        )
        .await
        {
            Ok(connection) => {
                tracing::info!(connection_id = %connection.id, name = %connection.name, "catalog connection created");
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(error = %error, "catalog connection creation failed");
                ExitCode::FAILURE
            }
        },
        Command::RegisterTable {
            organization,
            connection,
            namespace,
            table,
            database_url,
        } => match database::register_table(
            DatabaseUrl::new(database_url),
            &organization,
            connection,
            namespace,
            &table,
        )
        .await
        {
            Ok(identity) => {
                tracing::info!(table_id = %identity.id, table_uuid = %identity.table_uuid, "table registered");
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(error = %error, "table registration failed");
                ExitCode::FAILURE
            }
        },
        Command::BootstrapService {
            organization,
            display_name,
            table,
            database_url,
        } => match database::bootstrap_service(
            DatabaseUrl::new(database_url),
            &organization,
            &display_name,
            table.map(|_| Role::MaintenanceService),
            table,
        )
        .await
        {
            Ok(principal) => {
                tracing::info!(principal_id = %principal.id, "service principal created");
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(error = %error, "service principal creation failed");
                ExitCode::FAILURE
            }
        },
        Command::ServiceTokenVerifier { token_file } => {
            match std::fs::read_to_string(&token_file) {
                Ok(token) => match database::service_token_verifier(&token) {
                    Ok(verifier) => {
                        tracing::info!(verifier = %verifier, "service token verifier");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "service token is invalid");
                        ExitCode::FAILURE
                    }
                },
                Err(error) => {
                    tracing::error!(error = %error, "service token file could not be read");
                    ExitCode::FAILURE
                }
            }
        }
        Command::BootstrapAdmin {
            organization,
            issuer,
            subject,
            display_name,
            database_url,
        } => {
            let request = BootstrapAdministrator {
                organization,
                issuer,
                subject,
                display_name,
            };
            match bootstrap(&database_url, &request).await {
                Ok(principal_id) => {
                    tracing::info!(%principal_id, "administrator is provisioned");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    tracing::error!(error = %error, cause = ?std::error::Error::source(&error), "administrator bootstrap failed");
                    ExitCode::FAILURE
                }
            }
        }
        Command::GrantRole {
            organization,
            actor,
            issuer,
            subject,
            display_name,
            role,
            connection,
            table,
            database_url,
        } => {
            let scope = match (connection, table) {
                (Some(connection), _) => GrantScope::connection(connection),
                (None, Some(table)) => GrantScope::table(table),
                (None, None) => GrantScope::organization(),
            };
            let request = RoleAssignment {
                organization,
                actor,
                issuer,
                subject,
                display_name,
                role,
                scope,
            };
            match with_connection(&database_url, async |conn| {
                operator::grant_role(conn, &request).await
            })
            .await
            {
                Ok(grant) => {
                    tracing::info!(grant_id = %grant.id, principal_id = %grant.principal_id, role = %grant.role, "role granted");
                    ExitCode::SUCCESS
                }
                Err(error) => operator_failed(&error, "role grant failed"),
            }
        }
        Command::RevokeRole {
            organization,
            actor,
            grant,
            database_url,
        } => match with_connection(&database_url, async |conn| {
            operator::revoke_role(conn, &organization, actor, grant).await
        })
        .await
        {
            Ok(grant) => {
                tracing::info!(grant_id = %grant.id, principal_id = %grant.principal_id, role = %grant.role, "role revoked");
                ExitCode::SUCCESS
            }
            Err(error) => operator_failed(&error, "role revocation failed"),
        },
        Command::ListGrants {
            organization,
            actor,
            database_url,
        } => match with_connection(&database_url, async |conn| {
            operator::list_grants(conn, &organization, actor).await
        })
        .await
        {
            Ok(listings) => {
                for listing in listings {
                    let scope = listing.grant.scope;
                    tracing::info!(
                        grant_id = %listing.grant.id,
                        principal_id = %listing.principal.id,
                        issuer = %listing.principal.issuer,
                        subject = %listing.principal.subject,
                        display_name = %listing.principal.display_name,
                        role = %listing.grant.role,
                        connection = ?scope.catalog_connection.as_ref().map(ToString::to_string),
                        table = ?scope.table.as_ref().map(ToString::to_string),
                        "active grant"
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => operator_failed(&error, "grant listing failed"),
        },
        Command::AttestNonPublication {
            organization,
            actor,
            job,
            revision,
            reason,
            database_url,
        } => {
            let request = NonPublicationAttestation {
                organization,
                actor,
                job,
                revision,
                reason,
            };
            match with_connection(&database_url, async |conn| {
                operator::attest_non_publication(conn, &request).await
            })
            .await
            {
                Ok(job) => {
                    tracing::info!(job_id = %job.id, state = %job.state, revision = job.state_revision, "unknown publication outcome resolved");
                    ExitCode::SUCCESS
                }
                Err(error) => operator_failed(&error, "attestation failed"),
            }
        }
    }
}

fn load_config(
    path: &std::path::Path,
    database_url: Option<String>,
    http_bind: Option<String>,
    worker_bind: Option<String>,
) -> Result<CoordinatorConfig, bergsmith_coordinator::runtime::ConfigError> {
    let mut config = CoordinatorConfig::parse(path)?;
    if let Some(database_url) = database_url {
        config.database.url = Some(database_url);
        config.database.url_file = None;
    }
    if let Some(http_bind) = http_bind {
        config.http.bind = http_bind;
    }
    if let Some(worker_bind) = worker_bind {
        config.worker.bind = worker_bind;
    }
    config.validate()?;
    Ok(config)
}

async fn bootstrap(
    database_url: &str,
    request: &BootstrapAdministrator,
) -> Result<String, database::PersistenceError> {
    let database = database::connect(DatabaseUrl::new(database_url)).await?;
    let mut conn = database.pool().acquire().await?;
    Ok(database::bootstrap_administrator(&mut conn, request)
        .await?
        .to_string())
}

/// Parses a role a person may be granted.
fn parse_role(value: &str) -> Result<Role, String> {
    match value.to_ascii_lowercase().as_str() {
        "viewer" => Ok(Role::Viewer),
        "operator" => Ok(Role::Operator),
        "administrator" => Ok(Role::Administrator),
        _ => Err("expected viewer, operator, or administrator".to_owned()),
    }
}

/// Runs an operator command on one administration connection.
async fn with_connection<T>(
    database_url: &str,
    command: impl AsyncFnOnce(&mut operator::PgConnection) -> Result<T, OperatorError>,
) -> Result<T, OperatorError> {
    let database = database::connect(DatabaseUrl::new(database_url)).await?;
    let mut conn = database
        .pool()
        .acquire()
        .await
        .map_err(database::PersistenceError::from)?;
    command(&mut conn).await
}

fn operator_failed(error: &OperatorError, message: &'static str) -> ExitCode {
    tracing::error!(error = %error, cause = ?std::error::Error::source(error), "{message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_roles_for_people_parse_case_insensitively() {
        assert_eq!(parse_role("Viewer"), Ok(Role::Viewer));
        assert_eq!(parse_role("operator"), Ok(Role::Operator));
        assert_eq!(parse_role("ADMINISTRATOR"), Ok(Role::Administrator));
        assert!(parse_role("maintenanceservice").is_err());
    }

    #[test]
    fn the_command_line_is_consistent() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }
}
