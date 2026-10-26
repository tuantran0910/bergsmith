//! Notification payloads (BS-029.T1, ADR 0060).
//!
//! Every payload is built from an [`Event`]: what happened, to which resource,
//! and the stable reason the ledger recorded. Nothing else travels — no
//! credential, token, plan artifact, or table row — so a destination can be
//! handed to a third-party service without leaking execution detail.

use bergsmith_common::ids::OrganizationId;
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// One notification event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Envelope identifier; a receiver deduplicates delivery retries on it.
    pub id: Uuid,
    /// The stable event name: an [`EventKind`](bergsmith_common::notifications::EventKind)
    /// name for a rule-driven event, or `notification.test` for a test send.
    pub kind: String,
    /// When it happened, in UTC.
    pub occurred_at: DateTime<Utc>,
    /// The organization the event belongs to.
    pub organization_id: OrganizationId,
    /// One sentence for a human.
    pub summary: String,
    /// The entity the event is about.
    pub resource: ResourceRef,
    /// The stable reason code recorded with the event, if it has one.
    pub reason: Option<String>,
}

/// The resource an event is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRef {
    /// Resource type, such as `job`, `table`, `schedule`, or `worker_pool`.
    pub kind: String,
    /// Durable identifier, as text.
    pub id: String,
    /// Human label, when one exists.
    pub label: Option<String>,
}

impl ResourceRef {
    /// The label if present, otherwise the identifier.
    #[must_use]
    pub fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    id: Uuid,
    kind: &'a str,
    occurred_at: DateTime<Utc>,
    organization_id: OrganizationId,
    summary: &'a str,
    resource: Resource<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

#[derive(Serialize)]
struct Resource<'a> {
    kind: &'a str,
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
}

#[derive(Serialize)]
struct SlackBody<'a> {
    text: &'a str,
}

/// The webhook envelope as canonical JSON bytes.
///
/// # Errors
///
/// [`serde_json::Error`] when the event cannot be serialized, which the types
/// here make unreachable in practice.
pub fn webhook_body(event: &Event) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&Envelope {
        id: event.id,
        kind: &event.kind,
        occurred_at: event.occurred_at,
        organization_id: event.organization_id,
        summary: &event.summary,
        resource: Resource {
            kind: &event.resource.kind,
            id: &event.resource.id,
            label: event.resource.label.as_deref(),
        },
        reason: event.reason.as_deref(),
    })
}

/// The Slack-compatible body as canonical JSON bytes.
///
/// # Errors
///
/// [`serde_json::Error`] when the text cannot be serialized.
pub fn slack_body(event: &Event) -> Result<Vec<u8>, serde_json::Error> {
    let text = format!(
        "Bergsmith: {} ({} {})",
        event.summary,
        event.resource.kind,
        event.resource.display()
    );
    serde_json::to_vec(&SlackBody { text: &text })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Event {
        Event {
            id: Uuid::parse_str("018f0000-0000-7000-8000-000000000000").expect("uuid"),
            kind: "job.failed".to_owned(),
            occurred_at: DateTime::parse_from_rfc3339("2026-10-27T09:00:00Z")
                .expect("time")
                .with_timezone(&Utc),
            organization_id: OrganizationId::from_uuid(
                Uuid::parse_str("018f0000-0000-7000-8000-000000000001").expect("uuid"),
            ),
            summary: "Job 7 failed with AttemptLeaseLost".to_owned(),
            resource: ResourceRef {
                kind: "job".to_owned(),
                id: "018f0000-0000-7000-8000-000000000002".to_owned(),
                label: Some("nightly compaction".to_owned()),
            },
            reason: Some("attempt_lease_lost".to_owned()),
        }
    }

    #[test]
    fn the_envelope_carries_identifiers_and_the_reason() {
        let body = webhook_body(&event()).expect("encode");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["id"], "018f0000-0000-7000-8000-000000000000");
        assert_eq!(value["kind"], "job.failed");
        assert_eq!(value["occurred_at"], "2026-10-27T09:00:00Z");
        assert_eq!(
            value["organization_id"],
            "018f0000-0000-7000-8000-000000000001"
        );
        assert_eq!(value["summary"], "Job 7 failed with AttemptLeaseLost");
        assert_eq!(value["resource"]["kind"], "job");
        assert_eq!(value["resource"]["label"], "nightly compaction");
        assert_eq!(value["reason"], "attempt_lease_lost");
    }

    #[test]
    fn an_absent_reason_and_label_are_omitted() {
        let mut event = event();
        event.reason = None;
        event.resource.label = None;
        let value: serde_json::Value =
            serde_json::from_slice(&webhook_body(&event).expect("encode")).expect("json");
        assert!(value.get("reason").is_none());
        assert!(value["resource"].get("label").is_none());
    }

    #[test]
    fn a_slack_body_is_one_text_field() {
        let value: serde_json::Value =
            serde_json::from_slice(&slack_body(&event()).expect("encode")).expect("json");
        assert_eq!(
            value["text"],
            "Bergsmith: Job 7 failed with AttemptLeaseLost (job nightly compaction)"
        );
        assert_eq!(value.as_object().expect("object").len(), 1);
    }

    #[test]
    fn the_body_never_contains_credentials_or_plan_artifacts() {
        let body = String::from_utf8(webhook_body(&event()).expect("encode")).expect("utf8");
        for forbidden in ["password", "secret", "token", "credential", "manifest"] {
            assert!(!body.to_lowercase().contains(forbidden), "{forbidden}");
        }
    }
}
