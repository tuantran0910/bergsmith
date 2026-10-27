//! Notification destinations and outbound delivery (BS-029, ADR 0060).
//!
//! The coordinator may send events to configured webhook and Slack-compatible
//! endpoints. Three rules live here, and nowhere else:
//!
//! - A destination URL must be HTTPS and its host must match the deployment's
//!   allow-list, checked when a destination is written and again before every
//!   send.
//! - A destination's URL is read from a reference file under
//!   `notifications.secrets_dir` before a send (a Slack URL embeds a bearer
//!   token in its path), so the database and the audit trail hold only the
//!   reference.
//! - A webhook body is signed with an HMAC whose key is read from the same
//!   directory on each send. The database, the API, and the logs never hold the
//!   key.
//! - A payload carries identifiers and stable reason codes only: never a
//!   credential, a plan artifact, or a table row.
//!
//! This module performs no database work. The durable outbox that decides
//! *when* to send lives in the delivery loop (BS-029.T3).

pub mod client;
pub mod payload;
pub mod secret;
pub mod signature;

pub use client::{Delivery, DeliveryClient, DeliveryError, Target};
pub use payload::{Event, ResourceRef};
pub use secret::{SecretError, read_secret};
pub use signature::{SIGNATURE_TIMESTAMP, header_value, mac_hex};

use std::path::PathBuf;
use std::time::Duration;

use bergsmith_common::notifications::AllowList;

/// Deployment policy for outbound notifications.
#[derive(Clone, Debug)]
pub struct Settings {
    allow_list: AllowList,
    secrets_dir: Option<PathBuf>,
    timeout: Duration,
}

impl Settings {
    /// Builds the policy from validated configuration.
    #[must_use]
    pub fn new(allow_list: AllowList, secrets_dir: Option<PathBuf>, timeout: Duration) -> Self {
        Self {
            allow_list,
            secrets_dir,
            timeout,
        }
    }

    /// The hosts this deployment may contact.
    #[must_use]
    pub fn allow_list(&self) -> &AllowList {
        &self.allow_list
    }

    /// The directory holding secret and URL references, if one is configured.
    #[must_use]
    pub fn secrets_dir(&self) -> Option<&std::path::Path> {
        self.secrets_dir.as_deref()
    }

    /// The timeout for one delivery request.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}
