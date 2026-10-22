//! `bergsmith-coordinator` entry point. Startup wiring only; behavior lives in the
//! `bergsmith-coordinator` library crate.

use std::path::PathBuf;
use std::process::ExitCode;

use bergsmith_common::ids::{CatalogConnectionId, TableId};
use bergsmith_coordinator::database::{self, BootstrapAdministrator, DatabaseUrl};
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
}

#[expect(
    clippy::too_many_lines,
    reason = "one match arm per administration command keeps the CLI exhaustive"
)]
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

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
            table.map(|_| bergsmith_common::permission::Role::MaintenanceService),
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
