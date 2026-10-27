//! Notification destinations: kinds, event kinds, and the URL policy that
//! bounds where the coordinator may send (BS-029, ADR 0060).
//!
//! A destination is an organization-scoped webhook or Slack-compatible
//! endpoint. The deployment, not the database, decides which hosts are
//! reachable: `notifications.allowed_hosts` names them, and every destination
//! URL must match one entry. The check is pure so configuration parsing, the
//! API, and the delivery loop all use the same rule.
//!
//! Signing secrets are references, never values: a webhook stores a name that
//! the coordinator resolves from its secret directory at send time.

use std::net::IpAddr;

use url::{Host, Url};

/// The header carrying the webhook signature (ADR 0060).
pub const SIGNATURE_HEADER: &str = "x-bergsmith-signature";

/// The longest accepted destination URL, in bytes.
pub const MAX_URL_LENGTH: usize = 2048;

/// The longest accepted signing-secret reference, in bytes.
pub const MAX_SECRET_REF_LENGTH: usize = 128;

/// The port a destination URL uses when it names none, since HTTPS is required.
pub const DEFAULT_PORT: u16 = 443;

/// Where one notification is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DestinationKind {
    /// A generic JSON endpoint signed with the destination's secret.
    Webhook,
    /// A Slack-compatible incoming webhook that accepts one `text` field.
    Slack,
}

impl DestinationKind {
    /// Every kind, in a stable order.
    pub const ALL: &'static [Self] = &[Self::Webhook, Self::Slack];

    /// The stable name persisted in PostgreSQL and used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Slack => "slack",
        }
    }

    /// Parses the persisted name, or `None` if it is unrecognized.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }

    /// Whether a destination of this kind must name a signing secret.
    ///
    /// A webhook is always signed; a Slack-compatible body carries no
    /// Bergsmith signature because Slack does not verify one.
    #[must_use]
    pub const fn requires_signing_secret(self) -> bool {
        matches!(self, Self::Webhook)
    }
}

/// What a rule watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    /// A job reached `Failed`, or a task exhausted its retries.
    JobFailed,
    /// A job reached `ReplanRequired` and the plan can no longer be used.
    ReplanRequired,
    /// A published commit's outcome stayed unknown past the connection bound.
    CommitOutcomeUnknown,
    /// A schedule occurrence was skipped, missed, or coalesced away.
    OccurrenceMissed,
    /// A table's evaluated health became `Critical`.
    TableCritical,
    /// A worker pool has no live worker session while work is pending.
    PoolWithoutWorkers,
}

impl EventKind {
    /// Every event kind, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::JobFailed,
        Self::ReplanRequired,
        Self::CommitOutcomeUnknown,
        Self::OccurrenceMissed,
        Self::TableCritical,
        Self::PoolWithoutWorkers,
    ];

    /// The stable name used in rules, payloads, and the delivery log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobFailed => "job.failed",
            Self::ReplanRequired => "job.replan_required",
            Self::CommitOutcomeUnknown => "publication.commit_outcome_unknown",
            Self::OccurrenceMissed => "schedule.occurrence_missed",
            Self::TableCritical => "table.health_critical",
            Self::PoolWithoutWorkers => "workers.pool_without_workers",
        }
    }

    /// Parses the stable name, or `None` if it is unrecognized.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }

    /// Whether a rule for this kind may carry a repeat threshold.
    ///
    /// Repeats are meaningful for a condition that recurs (replans, missed
    /// occurrences, an unknown commit outcome that stays unknown); a job
    /// failure or a health transition is a single event.
    #[must_use]
    pub const fn allows_repeat_threshold(self) -> bool {
        matches!(
            self,
            Self::ReplanRequired | Self::CommitOutcomeUnknown | Self::OccurrenceMissed
        )
    }
}

/// Why a configured allow-list entry was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AllowListError {
    /// The entry was empty.
    #[error("an allowed host entry cannot be empty")]
    Empty,
    /// The entry was too long to be a host name.
    #[error("allowed host entry is longer than {max} characters")]
    TooLong {
        /// The accepted maximum.
        max: usize,
    },
    /// The entry carried a scheme, path, user info, or whitespace.
    #[error("allowed host entry {entry:?} must be a host with an optional port")]
    Malformed {
        /// The rejected entry.
        entry: String,
    },
    /// The port was zero or could not be parsed.
    #[error("allowed host entry {entry:?} has an invalid port")]
    InvalidPort {
        /// The rejected entry.
        entry: String,
    },
    /// The entry named a link-local address, which is never reachable.
    #[error("allowed host entry {entry:?} names a link-local address")]
    LinkLocal {
        /// The rejected entry.
        entry: String,
    },
}

/// The hosts this deployment may send notifications to.
///
/// An entry is a host with an optional port (`example.com`,
/// `hooks.example.com:8443`, `127.0.0.1:9000`, `[::1]:9000`) or a wildcard
/// subdomain match (`*.example.com`). The port defaults to
/// [`DEFAULT_PORT`] because only HTTPS destinations are accepted. Matching is
/// case-insensitive and never consults DNS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowList {
    entries: Vec<AllowEntry>,
}

/// One parsed allow-list entry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AllowEntry {
    host: EntryHost,
    port: u16,
}

/// The host part of an allow-list entry.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryHost {
    /// A domain name, optionally matching every subdomain.
    Domain { name: String, wildcard: bool },
    /// A literal address, compared exactly.
    Address(IpAddr),
}

impl AllowList {
    /// Parses every configured entry.
    ///
    /// An empty list is valid and allows no destination.
    ///
    /// # Errors
    ///
    /// [`AllowListError`] when an entry is not a bare host with an optional
    /// port, names a link-local address, or carries an invalid port.
    pub fn new<I, S>(entries: I) -> Result<Self, AllowListError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Vec::new();
        for entry in entries {
            parsed.push(AllowEntry::parse(entry.as_ref())?);
        }
        Ok(Self { entries: parsed })
    }

    /// Whether no host is allowed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `destination` names an allowed host and port.
    #[must_use]
    pub fn allows(&self, destination: &DestinationUrl) -> bool {
        let port = destination.port();
        let host = destination.host();
        self.entries
            .iter()
            .any(|entry| entry.port == port && entry.host.matches(&host))
    }
}

impl AllowEntry {
    fn parse(entry: &str) -> Result<Self, AllowListError> {
        if entry.is_empty() {
            return Err(AllowListError::Empty);
        }
        if entry.len() > MAX_URL_LENGTH {
            return Err(AllowListError::TooLong {
                max: MAX_URL_LENGTH,
            });
        }
        let malformed = || AllowListError::Malformed {
            entry: entry.to_owned(),
        };
        if entry.contains("://")
            || entry.contains('/')
            || entry.contains('?')
            || entry.contains('#')
            || entry.contains('@')
            || entry.chars().any(char::is_whitespace)
        {
            return Err(malformed());
        }
        let (host, port) = split_port(entry).map_err(|()| malformed())?;
        let port = match port {
            None => DEFAULT_PORT,
            Some(port) => port
                .parse::<u16>()
                .map_err(|_| AllowListError::InvalidPort {
                    entry: entry.to_owned(),
                })?,
        };
        if port == 0 {
            return Err(AllowListError::InvalidPort {
                entry: entry.to_owned(),
            });
        }
        let host = parse_host(host).map_err(|()| malformed())?;
        if let EntryHost::Address(address) = host
            && is_link_local(address)
        {
            return Err(AllowListError::LinkLocal {
                entry: entry.to_owned(),
            });
        }
        Ok(Self { host, port })
    }
}

impl EntryHost {
    fn matches(&self, host: &Host<&str>) -> bool {
        match (self, host) {
            (Self::Domain { name, wildcard }, Host::Domain(candidate)) => {
                let candidate = candidate.to_ascii_lowercase();
                if *wildcard {
                    candidate.len() > name.len() + 1 && candidate.ends_with(&format!(".{name}"))
                } else {
                    candidate == *name
                }
            }
            (Self::Address(address), Host::Ipv4(candidate)) => *address == IpAddr::V4(*candidate),
            (Self::Address(address), Host::Ipv6(candidate)) => *address == IpAddr::V6(*candidate),
            _ => false,
        }
    }
}

/// Splits a bracketed IPv6 host or a `host:port` pair.
fn split_port(entry: &str) -> Result<(&str, Option<&str>), ()> {
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(())?;
        if host.is_empty() {
            return Err(());
        }
        let port = match after {
            "" => None,
            after => Some(after.strip_prefix(':').ok_or(())?),
        };
        return Ok((host, port));
    }
    if entry.contains(':') {
        let (host, port) = entry.rsplit_once(':').ok_or(())?;
        if host.is_empty() || port.is_empty() {
            return Err(());
        }
        return Ok((host, Some(port)));
    }
    Ok((entry, None))
}

/// Parses a host into a domain or literal address.
fn parse_host(host: &str) -> Result<EntryHost, ()> {
    if host.is_empty() {
        return Err(());
    }
    if let Some(suffix) = host.strip_prefix("*.") {
        if suffix.is_empty() || suffix.contains('*') || suffix.parse::<IpAddr>().is_ok() {
            return Err(());
        }
        return Ok(EntryHost::Domain {
            name: suffix.to_ascii_lowercase(),
            wildcard: true,
        });
    }
    if host.contains('*') {
        return Err(());
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(EntryHost::Address(address));
    }
    Ok(EntryHost::Domain {
        name: host.to_ascii_lowercase(),
        wildcard: false,
    })
}

/// Whether `address` is a link-local address, including IPv4-mapped IPv6.
#[must_use]
pub fn is_link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.octets()[0] == 169 && v4.octets()[1] == 254,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_link_local(IpAddr::V4(v4));
            }
            let segments = v6.segments();
            segments[0] & 0xffc0 == 0xfe80
        }
    }
}

/// Why a destination URL was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DestinationUrlError {
    /// The URL did not parse.
    #[error("destination url is not a valid absolute url")]
    Malformed,
    /// The URL was longer than [`MAX_URL_LENGTH`].
    #[error("destination url is longer than {max} bytes")]
    TooLong {
        /// The accepted maximum.
        max: usize,
    },
    /// The URL was not HTTPS.
    #[error("destination url must use https")]
    NotHttps,
    /// The URL carried credentials in its authority.
    #[error("destination url must not carry user information")]
    UserInfo,
    /// The URL carried a fragment.
    #[error("destination url must not carry a fragment")]
    Fragment,
    /// The URL had no host.
    #[error("destination url must name a host")]
    MissingHost,
    /// The URL named a link-local address.
    #[error("destination url must not name a link-local address")]
    LinkLocal,
    /// The host did not match the deployment's allow-list.
    #[error("destination host is not in notifications.allowed_hosts")]
    NotAllowed,
}

/// A validated destination URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationUrl {
    url: Url,
}

impl DestinationUrl {
    /// Parses and validates a destination URL without consulting the
    /// allow-list.
    ///
    /// # Errors
    ///
    /// [`DestinationUrlError`] when the URL is malformed, not HTTPS, carries
    /// user info or a fragment, has no host, or names a link-local address.
    pub fn parse(raw: &str) -> Result<Self, DestinationUrlError> {
        if raw.len() > MAX_URL_LENGTH {
            return Err(DestinationUrlError::TooLong {
                max: MAX_URL_LENGTH,
            });
        }
        let url = Url::parse(raw).map_err(|_| DestinationUrlError::Malformed)?;
        if url.scheme() != "https" {
            return Err(DestinationUrlError::NotHttps);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(DestinationUrlError::UserInfo);
        }
        if url.fragment().is_some() {
            return Err(DestinationUrlError::Fragment);
        }
        let host = url.host().ok_or(DestinationUrlError::MissingHost)?;
        let address = match host {
            Host::Ipv4(address) => Some(IpAddr::V4(address)),
            Host::Ipv6(address) => Some(IpAddr::V6(address)),
            Host::Domain(_) => None,
        };
        if address.is_some_and(is_link_local) {
            return Err(DestinationUrlError::LinkLocal);
        }
        Ok(Self { url })
    }

    /// Parses a URL and requires an allow-list match.
    ///
    /// # Errors
    ///
    /// [`DestinationUrlError::NotAllowed`] when no entry matches, plus every
    /// parse error from [`Self::parse`].
    pub fn allow(&self, list: &AllowList) -> Result<(), DestinationUrlError> {
        if list.allows(self) {
            Ok(())
        } else {
            Err(DestinationUrlError::NotAllowed)
        }
    }

    /// The URL as configured, normalized by the URL parser.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// The effective port, defaulting to [`DEFAULT_PORT`].
    #[must_use]
    pub fn port(&self) -> u16 {
        self.url.port().unwrap_or(DEFAULT_PORT)
    }

    /// The host component.
    #[must_use]
    pub fn host(&self) -> Host<&str> {
        self.url.host().unwrap_or(Host::Domain(""))
    }
}

/// A validated secret reference: a bare file name under
/// `notifications.secrets_dir`. A reference names a webhook's signing key or a
/// destination URL, so the credential-bearing value never enters PostgreSQL, an
/// audit record, or a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretRef(String);

impl SecretRef {
    /// Validates a reference.
    ///
    /// A reference is 1 to [`MAX_SECRET_REF_LENGTH`] bytes of ASCII letters,
    /// digits, `.`, `_`, and `-`, and is neither `.` nor `..`, so it can never
    /// leave the configured directory.
    ///
    /// # Errors
    ///
    /// [`SecretRefError`] when the name violates those rules.
    pub fn parse(raw: &str) -> Result<Self, SecretRefError> {
        if raw.is_empty() || raw.len() > MAX_SECRET_REF_LENGTH {
            return Err(SecretRefError::Length {
                max: MAX_SECRET_REF_LENGTH,
            });
        }
        if raw == "." || raw == ".." {
            return Err(SecretRefError::Reserved);
        }
        if !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(SecretRefError::Character);
        }
        Ok(Self(raw.to_owned()))
    }

    /// The reference as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a secret reference was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretRefError {
    /// The name was empty or longer than [`MAX_SECRET_REF_LENGTH`].
    #[error("a secret reference is 1 to {max} bytes")]
    Length {
        /// The accepted maximum.
        max: usize,
    },
    /// The name was `.` or `..`.
    #[error("a secret reference cannot be a directory")]
    Reserved,
    /// The name contained a character outside `[A-Za-z0-9._-]`.
    #[error("a secret reference uses only ASCII letters, digits, and ._-")]
    Character,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(entries: &[&str]) -> AllowList {
        AllowList::new(entries).expect("valid allow-list")
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in DestinationKind::ALL {
            assert_eq!(DestinationKind::from_name(kind.as_str()), Some(*kind));
        }
        assert_eq!(DestinationKind::from_name("email"), None);
        assert!(DestinationKind::Webhook.requires_signing_secret());
        assert!(!DestinationKind::Slack.requires_signing_secret());
    }

    #[test]
    fn event_names_round_trip() {
        for kind in EventKind::ALL {
            assert_eq!(EventKind::from_name(kind.as_str()), Some(*kind));
        }
        assert_eq!(EventKind::from_name("job.never"), None);
        assert!(EventKind::ReplanRequired.allows_repeat_threshold());
        assert!(!EventKind::JobFailed.allows_repeat_threshold());
    }

    #[test]
    fn only_https_urls_are_accepted() {
        assert_eq!(
            DestinationUrl::parse("http://hooks.example.com/x"),
            Err(DestinationUrlError::NotHttps)
        );
        assert!(DestinationUrl::parse("https://hooks.example.com/x").is_ok());
    }

    #[test]
    fn user_info_and_fragments_are_rejected() {
        assert_eq!(
            DestinationUrl::parse("https://user:pass@hooks.example.com/x"),
            Err(DestinationUrlError::UserInfo)
        );
        assert_eq!(
            DestinationUrl::parse("https://hooks.example.com/x#frag"),
            Err(DestinationUrlError::Fragment)
        );
    }

    #[test]
    fn link_local_destinations_are_rejected() {
        for raw in [
            "https://169.254.169.254/latest",
            "https://[::ffff:169.254.169.254]/latest",
            "https://[fe80::1]/hook",
        ] {
            assert_eq!(
                DestinationUrl::parse(raw),
                Err(DestinationUrlError::LinkLocal),
                "{raw}"
            );
        }
    }

    #[test]
    fn link_local_entries_are_rejected_by_configuration() {
        for entry in ["169.254.169.254", "[fe80::1]:8443"] {
            assert!(
                matches!(
                    AllowList::new([entry]),
                    Err(AllowListError::LinkLocal { .. })
                ),
                "{entry}"
            );
        }
    }

    #[test]
    fn exact_host_and_port_must_match() {
        let list = allow(&["hooks.example.com"]);
        let exact = DestinationUrl::parse("https://hooks.example.com/services").expect("url");
        assert!(list.allows(&exact));
        let other_port =
            DestinationUrl::parse("https://hooks.example.com:8443/services").expect("url");
        assert!(!list.allows(&other_port));
        let other_host = DestinationUrl::parse("https://evil.example.org/services").expect("url");
        assert!(!list.allows(&other_host));
        assert_eq!(
            other_host.allow(&list),
            Err(DestinationUrlError::NotAllowed)
        );
    }

    #[test]
    fn an_empty_list_allows_nothing() {
        let list = AllowList::new(Vec::<String>::new()).expect("empty list");
        assert!(list.is_empty());
        let url = DestinationUrl::parse("https://hooks.example.com/x").expect("url");
        assert!(!list.allows(&url));
    }

    #[test]
    fn wildcard_entries_match_subdomains_only() {
        let list = allow(&["*.example.com"]);
        assert!(list.allows(&DestinationUrl::parse("https://hooks.example.com/x").expect("url")));
        assert!(
            list.allows(&DestinationUrl::parse("https://deep.hooks.example.com/x").expect("url"))
        );
        assert!(!list.allows(&DestinationUrl::parse("https://example.com/x").expect("url")));
        assert!(!list.allows(&DestinationUrl::parse("https://badexample.com/x").expect("url")));
    }

    #[test]
    fn hosts_match_case_insensitively() {
        let list = allow(&["Hooks.Example.COM:8443"]);
        let url = DestinationUrl::parse("https://HOOKS.example.com:8443/x").expect("url");
        assert!(list.allows(&url));
    }

    #[test]
    fn loopback_is_allowed_only_when_named() {
        let url = DestinationUrl::parse("https://127.0.0.1:9000/hook").expect("url");
        assert!(!allow(&["127.0.0.1"]).allows(&url));
        assert!(allow(&["127.0.0.1:9000"]).allows(&url));
        let v6 = DestinationUrl::parse("https://[::1]:9000/hook").expect("url");
        assert!(allow(&["[::1]:9000"]).allows(&v6));
    }

    #[test]
    fn malformed_entries_are_rejected() {
        for entry in [
            "",
            "https://example.com",
            "example.com/path",
            "user@example.com",
            "exa mple.com",
            "example.com:0",
            "*.example.com:port",
            "*.*.example.com",
            "[::1",
        ] {
            assert!(
                AllowList::new([entry]).is_err(),
                "entry {entry:?} should be rejected"
            );
        }
    }

    #[test]
    fn secret_references_can_never_leave_the_directory() {
        assert_eq!(
            SecretRef::parse("signing-key.pem").expect("ref").as_str(),
            "signing-key.pem"
        );
        for raw in [
            "",
            ".",
            "..",
            "../key",
            "a/b",
            "key value",
            &"a".repeat(129),
        ] {
            assert!(SecretRef::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn default_port_applies_when_the_url_omits_it() {
        let url = DestinationUrl::parse("https://hooks.example.com/x").expect("url");
        assert_eq!(url.port(), DEFAULT_PORT);
    }
}
