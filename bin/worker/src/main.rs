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
    /// Run the distributed supervisor with a private durable state directory.
    Run {
        /// Non-secret worker-pool JSON configuration.
        #[arg(long)]
        config: PathBuf,
        /// Pool-scoped service token file; never passed to task subprocesses.
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Execute one granted task using a private supervisor socket.
    Exec {
        /// Per-attempt Unix domain socket.
        #[arg(long)]
        socket: PathBuf,
        /// Expected supervisor PID for parent-death race detection.
        #[arg(long)]
        parent_pid: u32,
        /// Prepared Linux cgroup; bootstrap re-executes before runtime startup.
        #[cfg(target_os = "linux")]
        #[arg(long, hide = true)]
        bootstrap_cgroup: Option<PathBuf>,
    },
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

fn main() -> ExitCode {
    let command = Cli::parse().command;
    #[cfg(target_os = "linux")]
    if let Command::Exec {
        socket,
        parent_pid,
        bootstrap_cgroup: Some(cgroup),
    } = &command
    {
        let _ = bergsmith_worker::supervision::bootstrap(socket, *parent_pid, cgroup);
        return ExitCode::FAILURE;
    }
    if let Command::Exec { parent_pid, .. } = &command
        && bergsmith_worker::supervision::prepare_subprocess(*parent_pid).is_err()
    {
        return ExitCode::FAILURE;
    }
    if let Err(format) = init_logging() {
        tracing::error!(format, "BERGSMITH_LOG_FORMAT must be text or json");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "worker runtime could not start");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(dispatch(command));
    // A cancelled native blocking operation must not delay process termination.
    runtime.shutdown_background();
    result
}

async fn dispatch(command: Command) -> ExitCode {
    match command {
        Command::Run { config, token_file } => {
            match Box::pin(bergsmith_worker::supervision::serve(&config, &token_file)).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(error = %error, "worker supervision failed");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Exec { socket, .. } => {
            match bergsmith_worker::supervision::execute(&socket).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(error = %error, "task subprocess failed");
                    ExitCode::FAILURE
                }
            }
        }
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
                    tracing::error!(
                        error = %error,
                        reason_code = error.reason_code(),
                        disposition = error.disposition().as_str(),
                        "local execution failed"
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
}
