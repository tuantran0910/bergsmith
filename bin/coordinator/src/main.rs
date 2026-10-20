//! `bergsmith-coordinator` entry point. Startup wiring only; behavior lives in the
//! `bergsmith-coordinator` library crate.

use std::process::ExitCode;

use bergsmith_coordinator::database::{self, BootstrapAdministrator, DatabaseUrl};
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
