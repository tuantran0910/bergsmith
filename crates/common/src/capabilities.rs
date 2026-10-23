//! What a catalog connection can do, as recorded by a capability check.
//!
//! A check asks one catalog what it advertises. The answer is diagnostic: it
//! tells an operator whether a connection can serve the work Bergsmith needs,
//! and it never changes a connection's configuration.

use serde::{Deserialize, Serialize};

use crate::digest::Sha256Digest;

/// The domain a routing fingerprint is hashed under.
const ROUTING_DOMAIN: &[u8] = b"bergsmith:catalog-routing";

/// A fingerprint of the settings a capability check depends on.
///
/// Two connections with the same fingerprint are configured the same way, so
/// a recorded answer describes both. A routing edit changes the fingerprint,
/// and an answer recorded under the old one is stale.
///
/// Every field is tagged and length-prefixed, so an absent setting and an
/// empty one never hash alike: a connection without a warehouse sends no
/// warehouse parameter, while one with an empty warehouse sends an empty
/// value.
#[must_use]
pub fn routing_fingerprint(
    uri: &str,
    warehouse: Option<&str>,
    credential_ref: Option<&str>,
) -> String {
    let mut bytes = Vec::new();
    append_field(&mut bytes, Some(uri));
    append_field(&mut bytes, warehouse);
    append_field(&mut bytes, credential_ref);
    Sha256Digest::of(ROUTING_DOMAIN, &bytes).to_hex()
}

/// Appends one tagged, length-prefixed field.
fn append_field(bytes: &mut Vec<u8>, value: Option<&str>) {
    match value {
        None => bytes.push(0),
        Some(value) => {
            bytes.push(1);
            bytes.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
    }
}

/// Whether one catalog requirement is met, missing, or not yet proven.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    /// The catalog advertises what the requirement needs.
    Met,
    /// The catalog does not advertise it.
    Missing,
    /// The catalog configuration cannot show it. Another operation, such as
    /// loading a table, proves it.
    Unknown,
}

impl CapabilityState {
    /// The stable name used in the API and in stored records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }

    /// The state as its stable name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "met" => Some(Self::Met),
            "missing" => Some(Self::Missing),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// What one catalog advertised at its last check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionCapabilities {
    /// Whether the catalog lists the tables of a namespace.
    pub list_tables: CapabilityState,
    /// Whether the catalog loads a table's metadata.
    pub load_table: CapabilityState,
    /// Whether the catalog accepts a commit.
    pub commit_table: CapabilityState,
    /// Whether the catalog vends table-scoped storage credentials.
    pub vended_credentials: CapabilityState,
    /// Whether the catalog can refresh vended credentials on request.
    pub credentials_endpoint: CapabilityState,
    /// How long the catalog remembers idempotency keys, in seconds.
    ///
    /// The value is the advertised one, without a bound: a catalog may
    /// advertise a lifetime beyond what a browser can count exactly, so the
    /// API sends it as a decimal string.
    pub idempotency_key_lifetime_seconds: Option<u64>,
}

impl ConnectionCapabilities {
    /// Whether every endpoint Bergsmith cannot work without is present.
    ///
    /// Vended credentials and the credentials endpoint are not part of this
    /// answer: they are proved by loading a table, not by the configuration.
    #[must_use]
    pub const fn complete(&self) -> bool {
        matches!(self.list_tables, CapabilityState::Met)
            && matches!(self.load_table, CapabilityState::Met)
            && matches!(self.commit_table, CapabilityState::Met)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_setting_and_an_empty_one_differ() {
        let absent = routing_fingerprint("https://catalog.example", None, None);
        let empty = routing_fingerprint("https://catalog.example", Some(""), Some(""));
        assert_ne!(absent, empty);
    }

    #[test]
    fn the_same_settings_fingerprint_the_same() {
        assert_eq!(
            routing_fingerprint(
                "https://catalog.example",
                Some("main"),
                Some("secret/catalog")
            ),
            routing_fingerprint(
                "https://catalog.example",
                Some("main"),
                Some("secret/catalog")
            )
        );
    }

    #[test]
    fn every_setting_changes_the_fingerprint() {
        let base = routing_fingerprint("https://catalog.example", Some("main"), Some("secret"));
        assert_ne!(
            base,
            routing_fingerprint("https://other.example", Some("main"), Some("secret"))
        );
        assert_ne!(
            base,
            routing_fingerprint("https://catalog.example", Some("other"), Some("secret"))
        );
        assert_ne!(
            base,
            routing_fingerprint("https://catalog.example", Some("main"), Some("other"))
        );
        assert_ne!(
            base,
            routing_fingerprint("https://catalog.example", None, Some("secret"))
        );
    }

    #[test]
    fn a_field_cannot_be_confused_with_another() {
        // Without a tag and a length, "ab" + "c" and "a" + "bc" would hash
        // alike.
        assert_ne!(
            routing_fingerprint("https://catalog.example", Some("ab"), Some("c")),
            routing_fingerprint("https://catalog.example", Some("a"), Some("bc"))
        );
    }
}
