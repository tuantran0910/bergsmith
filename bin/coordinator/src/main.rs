//! `bergsmith-coordinator` entry point. Startup wiring only; behavior lives in the
//! `bergsmith-coordinator` library crate.

use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "bergsmith-coordinator is not implemented yet"
    );
}
