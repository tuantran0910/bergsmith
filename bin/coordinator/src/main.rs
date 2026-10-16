//! `bergsmith-coordinator` entry point. Startup wiring only; behavior lives in the
//! `bergsmith-coordinator` library crate.

use std::process::ExitCode;

use bergsmith_coordinator::database::{self, DatabaseUrl};
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
    }
}
