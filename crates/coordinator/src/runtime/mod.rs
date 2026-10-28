//! The coordinator `serve` runtime (BS-020.T1).
//!
//! This module composes the library services into one process: leader
//! election, the worker RPC endpoint, the scheduler, the planning and
//! publication loops, and the browser API plus static assets on one origin.
//!
//! The binary is only startup wiring; every behavior here is testable without
//! a subprocess.

pub mod catalogs;
pub mod config;
pub mod http;
pub mod planning;
pub mod publication;
pub mod serve;
pub mod statistics;

pub use catalogs::{AccessError, RuntimeCatalogs};
pub use config::{ConfigError, CoordinatorConfig};
pub use http::{HttpError, app};
pub use planning::{PlannerService, PlanningSummary};
pub use publication::{PublicationService, PublicationSummary};
pub use serve::{ServeError, serve};
pub use statistics::{SampleError, SampleSummary, SamplingSettings, StatisticsCollector};

/// Bounded background storage reporting.
pub mod storage_breakdown;
