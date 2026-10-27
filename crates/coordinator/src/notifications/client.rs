//! The outbound HTTP client for notification destinations (BS-029.T1,
//! ADR 0060).
//!
//! One `reqwest` client is built once per coordinator: HTTPS only, no
//! redirects, a bounded timeout, and no proxy environment. `send` re-checks
//! the destination against the current allow-list, resolves the signing secret
//! for a webhook, and reports the response status and a short excerpt. It never
//! follows a destination to another host and never puts the destination URL in
//! an error, because a Slack URL embeds its token in the path.

use std::path::Path;

use bergsmith_common::notifications::{
    DestinationKind, DestinationUrl, DestinationUrlError, SIGNATURE_HEADER,
};
use chrono::Utc;
use reqwest::header::CONTENT_TYPE;

use super::Settings;
use super::payload::{self, Event};
use super::secret::{SecretError, read_secret};
use super::signature::header_value;

/// How much of a response body is kept for the delivery log.
pub const MAX_RESPONSE_BYTES: usize = 1024;

/// Where one event is sent.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    /// Transport shape.
    pub kind: DestinationKind,
    /// The destination URL, resolved from its reference before the send.
    pub url: &'a str,
    /// The stored signing-secret reference; required for a webhook.
    pub signing_secret_ref: Option<&'a str>,
}

/// What a send produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// The HTTP status code.
    pub status: u16,
    /// Whether the status was 2xx.
    pub delivered: bool,
    /// Up to [`MAX_RESPONSE_BYTES`] of the response body, when any.
    pub response: Option<String>,
}

/// Why a send failed before a status was available.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// The destination URL is invalid or not allowed.
    #[error("{0}")]
    InvalidUrl(#[from] DestinationUrlError),
    /// A webhook destination has no signing-secret reference.
    #[error("webhook destination has no signing secret reference")]
    MissingSecret,
    /// The signing secret could not be resolved.
    #[error("{0}")]
    Secret(#[from] SecretError),
    /// The payload could not be encoded.
    #[error("payload could not be encoded: {0}")]
    Encode(#[source] serde_json::Error),
    /// The request could not be completed.
    #[error("request failed: {reason}")]
    Transport {
        /// The failure, with the URL removed.
        reason: String,
    },
    /// The HTTP client could not be built.
    #[error("http client could not be built: {reason}")]
    Client {
        /// The builder failure.
        reason: String,
    },
}

/// Sends notification payloads within the deployment's policy.
#[derive(Clone, Debug)]
pub struct DeliveryClient {
    http: reqwest::Client,
    allow_list: bergsmith_common::notifications::AllowList,
    secrets_dir: Option<std::path::PathBuf>,
}

impl DeliveryClient {
    /// Builds the client from validated settings.
    ///
    /// # Errors
    ///
    /// [`DeliveryError::Client`] when the TLS client cannot be built.
    pub fn new(settings: &Settings) -> Result<Self, DeliveryError> {
        let http = reqwest::Client::builder()
            .timeout(settings.timeout())
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .user_agent(concat!("bergsmith-coordinator/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| DeliveryError::Client {
                reason: error.without_url().to_string(),
            })?;
        Ok(Self {
            http,
            allow_list: settings.allow_list().clone(),
            secrets_dir: settings.secrets_dir().map(Path::to_path_buf),
        })
    }

    /// The hosts this deployment may contact.
    #[must_use]
    pub fn allow_list(&self) -> &bergsmith_common::notifications::AllowList {
        &self.allow_list
    }

    /// The directory holding secret references, if one is configured.
    #[must_use]
    pub fn secrets_dir(&self) -> Option<&Path> {
        self.secrets_dir.as_deref()
    }

    /// Sends one event.
    ///
    /// # Errors
    ///
    /// [`DeliveryError`] when the URL is invalid or not allowed, a webhook's
    /// secret cannot be resolved, the payload cannot be encoded, or the request
    /// cannot complete.
    pub async fn send(
        &self,
        target: &Target<'_>,
        event: &Event,
    ) -> Result<Delivery, DeliveryError> {
        let destination = DestinationUrl::parse(target.url)?;
        destination.allow(&self.allow_list)?;
        let body = match target.kind {
            DestinationKind::Webhook => {
                payload::webhook_body(event).map_err(DeliveryError::Encode)?
            }
            DestinationKind::Slack => payload::slack_body(event).map_err(DeliveryError::Encode)?,
        };
        let mut request = self
            .http
            .post(destination.as_str())
            .header(CONTENT_TYPE, "application/json");
        if target.kind.requires_signing_secret() {
            let reference = target
                .signing_secret_ref
                .ok_or(DeliveryError::MissingSecret)?;
            let secret = read_secret(self.secrets_dir.as_deref(), reference)
                .await
                .map_err(DeliveryError::Secret)?;
            let signature = header_value(secret.expose().as_bytes(), Utc::now().timestamp(), &body);
            request = request.header(SIGNATURE_HEADER, signature);
        }
        let mut response = request.body(body).send().await.map_err(transport)?;
        let status = response.status().as_u16();
        let mut excerpt = Vec::new();
        while excerpt.len() < MAX_RESPONSE_BYTES {
            let Some(chunk) = response.chunk().await.map_err(transport)? else {
                break;
            };
            let take = (MAX_RESPONSE_BYTES - excerpt.len()).min(chunk.len());
            excerpt.extend_from_slice(&chunk[..take]);
        }
        let response = String::from_utf8_lossy(&excerpt).trim().to_owned();
        Ok(Delivery {
            status,
            delivered: (200..300).contains(&status),
            response: (!response.is_empty()).then_some(response),
        })
    }
}

fn transport(error: reqwest::Error) -> DeliveryError {
    DeliveryError::Transport {
        reason: error.without_url().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bergsmith_common::ids::OrganizationId;
    use bergsmith_common::notifications::AllowList;
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::notifications::payload::ResourceRef;

    fn settings(hosts: &[&str], secrets_dir: Option<std::path::PathBuf>) -> Settings {
        Settings::new(
            AllowList::new(hosts).expect("allow-list"),
            secrets_dir,
            Duration::from_secs(5),
        )
    }

    fn event() -> Event {
        Event {
            id: Uuid::now_v7(),
            kind: "job.failed".to_owned(),
            occurred_at: Utc::now(),
            organization_id: OrganizationId::generate(),
            summary: "test".to_owned(),
            resource: ResourceRef {
                kind: "job".to_owned(),
                id: Uuid::now_v7().to_string(),
                label: None,
            },
            reason: None,
        }
    }

    #[tokio::test]
    async fn a_non_https_url_is_rejected_before_any_request() {
        let client = DeliveryClient::new(&settings(&["127.0.0.1:9"], None)).expect("client");
        let target = Target {
            kind: DestinationKind::Slack,
            url: "http://127.0.0.1:9/hook",
            signing_secret_ref: None,
        };
        assert!(matches!(
            client.send(&target, &event()).await,
            Err(DeliveryError::InvalidUrl(DestinationUrlError::NotHttps))
        ));
    }

    #[tokio::test]
    async fn a_host_outside_the_allow_list_is_rejected() {
        let client = DeliveryClient::new(&settings(&["hooks.example.com"], None)).expect("client");
        let target = Target {
            kind: DestinationKind::Slack,
            url: "https://evil.example.org/hook",
            signing_secret_ref: None,
        };
        assert!(matches!(
            client.send(&target, &event()).await,
            Err(DeliveryError::InvalidUrl(DestinationUrlError::NotAllowed))
        ));
    }

    #[tokio::test]
    async fn a_webhook_without_a_resolvable_secret_is_never_sent() {
        let client = DeliveryClient::new(&settings(&["127.0.0.1:9"], None)).expect("client");
        let target = Target {
            kind: DestinationKind::Webhook,
            url: "https://127.0.0.1:9/hook",
            signing_secret_ref: Some("key"),
        };
        assert!(matches!(
            client.send(&target, &event()).await,
            Err(DeliveryError::Secret(SecretError::NotConfigured))
        ));
        let target = Target {
            kind: DestinationKind::Webhook,
            url: "https://127.0.0.1:9/hook",
            signing_secret_ref: None,
        };
        assert!(matches!(
            client.send(&target, &event()).await,
            Err(DeliveryError::MissingSecret)
        ));
    }
}
