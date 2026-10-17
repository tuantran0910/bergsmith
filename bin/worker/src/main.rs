//! `bergsmith-worker` entry point. Startup wiring only; behavior lives in the
//! `bergsmith-worker` library crate.

use std::path::PathBuf;
use std::process::ExitCode;

use bergsmith_worker::local::{self, LocalRequest};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "bergsmith-worker", version, about = "Bergsmith worker")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Execute a pinned local plan and write receipts, without publishing.
    Local {
        /// Immutable plan JSON.
        #[arg(long)]
        plan: PathBuf,
        /// Trusted plan digest, supplied separately from the file.
        #[arg(long)]
        plan_sha256: String,
        /// New directory for receipt protobufs and summary.json.
        #[arg(long)]
        receipts: PathBuf,
        /// Optional execution-limit JSON.
        #[arg(long)]
        limits: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match Cli::parse().command {
        Command::Local {
            plan,
            plan_sha256,
            receipts,
            limits,
        } => {
            match local::run(LocalRequest {
                plan_file: plan,
                plan_sha256,
                receipt_dir: receipts,
                limits_file: limits,
            })
            .await
            {
                Ok(run) => {
                    tracing::info!(
                        tasks = run.receipts.len(),
                        elapsed_millis = run.elapsed_millis,
                        "local execution complete; outputs are unpublished"
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    tracing::error!(error = %error, "local execution failed");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
