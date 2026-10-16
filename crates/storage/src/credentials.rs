//! Storage credentials and their refreshable source.
//!
//! The MVP's only credential source is the catalog: credentials vended for a
//! table, scoped to its location, and refreshed before they expire. Workers
//! receive them in grants and heartbeats and install them in a
//! [`CredentialCell`], which `FileIO` consults on every signed request, so a
//! refresh takes effect without rebuilding any reader or writer.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bergsmith_common::secret::Secret;
use iceberg::io::{S3Credential, StorageCredentialKind, StorageCredentialProvider};

use crate::location::{Backend, Location, LocationError};

const S3_ACCESS_KEY_ID: &str = "s3.access-key-id";
const S3_SECRET_ACCESS_KEY: &str = "s3.secret-access-key";
const S3_SESSION_TOKEN: &str = "s3.session-token";
const S3_SESSION_TOKEN_EXPIRES_AT_MS: &str = "s3.session-token-expires-at-ms";
const EXPIRATION_TIME_MS: &str = "expiration-time";

/// Backend-specific authentication material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// S3 access keys with an optional session token.
    S3 {
        /// The access key ID. Not secret on its own, but never logged either.
        access_key_id: Secret,
        /// The secret access key.
        secret_access_key: Secret,
        /// The STS session token, for temporary credentials.
        session_token: Option<Secret>,
    },
}

/// A credential that grants access under one location prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageCredential {
    /// The prefix the credential is scoped to.
    pub prefix: Location,
    /// The authentication material.
    pub kind: CredentialKind,
    /// When the credential stops working, if it expires.
    pub expires_at: Option<SystemTime>,
}

/// Why vended credential properties could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The credential prefix is not a valid location.
    #[error("credential prefix is not a valid location")]
    InvalidPrefix(#[source] LocationError),
    /// Credentials for this backend are not supported yet.
    #[error("credentials for {0:?} storage are not supported")]
    UnsupportedBackend(Backend),
    /// A required property is missing.
    #[error("vended credential is missing {0}")]
    MissingProperty(&'static str),
    /// A property has a malformed value.
    #[error("vended credential property {0} is malformed")]
    MalformedProperty(&'static str),
    /// No credential covers the location.
    #[error("no storage credential covers {0}")]
    NotCovered(String),
    /// Every covering credential expires too soon.
    #[error("storage credential for {0} expires too soon")]
    Expiring(String),
}

impl StorageCredential {
    /// Builds a credential from the properties of one vended
    /// `storage-credentials` entry (or a table `config` map), keeping only
    /// the authentication keys.
    ///
    /// # Errors
    ///
    /// A [`CredentialError`] for an invalid prefix, an unsupported backend,
    /// or missing or malformed properties.
    pub fn from_vended(
        prefix: &str,
        config: &BTreeMap<String, String>,
    ) -> Result<Self, CredentialError> {
        let prefix = Location::parse(prefix).map_err(CredentialError::InvalidPrefix)?;
        if prefix.backend() != Backend::S3 {
            return Err(CredentialError::UnsupportedBackend(prefix.backend()));
        }
        let required = |key: &'static str| {
            config
                .get(key)
                .filter(|value| !value.is_empty())
                .map(Secret::new)
                .ok_or(CredentialError::MissingProperty(key))
        };
        let kind = CredentialKind::S3 {
            access_key_id: required(S3_ACCESS_KEY_ID)?,
            secret_access_key: required(S3_SECRET_ACCESS_KEY)?,
            session_token: config.get(S3_SESSION_TOKEN).map(Secret::new),
        };
        let expires_at = [S3_SESSION_TOKEN_EXPIRES_AT_MS, EXPIRATION_TIME_MS]
            .into_iter()
            .find_map(|key| config.get(key).map(|value| (key, value)))
            .map(|(key, value)| {
                value
                    .parse::<u64>()
                    .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
                    .map_err(|_| CredentialError::MalformedProperty(key))
            })
            .transpose()?;
        Ok(Self {
            prefix,
            kind,
            expires_at,
        })
    }

    /// Whether the credential grants access to `location`.
    #[must_use]
    pub fn covers(&self, location: &Location) -> bool {
        location.is_within(&self.prefix)
    }

    /// Whether the credential stays valid for longer than `validity` after `now`.
    #[must_use]
    pub fn valid_for(&self, now: SystemTime, validity: Duration) -> bool {
        self.expires_at
            .is_none_or(|expires_at| now + validity < expires_at)
    }

    fn to_iceberg(&self) -> iceberg::io::StorageCredential {
        let CredentialKind::S3 {
            access_key_id,
            secret_access_key,
            session_token,
        } = &self.kind;
        let credential =
            iceberg::io::StorageCredential::new(StorageCredentialKind::S3(S3Credential::new(
                access_key_id.expose(),
                secret_access_key.expose(),
                session_token
                    .as_ref()
                    .map(|token| token.expose().to_owned()),
            )))
            .with_prefix(self.prefix.to_string());
        match self.expires_at {
            Some(expires_at) => credential.with_expiration(expires_at),
            None => credential,
        }
    }
}

/// The current credentials for one table, shared with `FileIO`.
///
/// Cloning shares the same cell. [`replace`](Self::replace) installs
/// refreshed credentials for every reader and writer at once.
#[derive(Clone, Debug, Default)]
pub struct CredentialCell {
    credentials: Arc<RwLock<Vec<StorageCredential>>>,
}

impl CredentialCell {
    /// Creates a cell holding `credentials`.
    #[must_use]
    pub fn new(credentials: Vec<StorageCredential>) -> Self {
        Self {
            credentials: Arc::new(RwLock::new(credentials)),
        }
    }

    /// Replaces every credential, as after a refresh.
    pub fn replace(&self, credentials: Vec<StorageCredential>) {
        // A poisoned lock still holds a whole Vec; recover and overwrite it.
        let mut guard = self
            .credentials
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = credentials;
    }

    /// Selects the most specific credential covering `location` that stays
    /// valid for longer than `validity`.
    ///
    /// # Errors
    ///
    /// [`CredentialError::NotCovered`] when no credential covers the
    /// location and [`CredentialError::Expiring`] when every covering
    /// credential expires too soon.
    pub fn select(
        &self,
        location: &Location,
        validity: Duration,
    ) -> Result<StorageCredential, CredentialError> {
        let guard = self
            .credentials
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut covering: Vec<&StorageCredential> =
            guard.iter().filter(|c| c.covers(location)).collect();
        if covering.is_empty() {
            return Err(CredentialError::NotCovered(location.to_string()));
        }
        covering.sort_by_key(|c| std::cmp::Reverse(c.prefix.key().len()));
        let now = SystemTime::now();
        covering
            .into_iter()
            .find(|c| c.valid_for(now, validity))
            .cloned()
            .ok_or_else(|| CredentialError::Expiring(location.to_string()))
    }
}

#[async_trait]
impl StorageCredentialProvider for CredentialCell {
    fn supports_path(&self, path: &str) -> bool {
        Location::parse(path).is_ok_and(|location| location.backend() == Backend::S3)
    }

    async fn load_credential_with_minimum_validity(
        &self,
        path: &str,
        minimum_validity: Duration,
    ) -> iceberg::Result<iceberg::io::StorageCredential> {
        let location = Location::parse(path).map_err(|error| {
            iceberg::Error::new(iceberg::ErrorKind::DataInvalid, "invalid storage location")
                .with_source(error)
        })?;
        self.select(&location, minimum_validity)
            .map(|credential| credential.to_iceberg())
            .map_err(|error| {
                iceberg::Error::new(
                    iceberg::ErrorKind::Unexpected,
                    "no usable storage credential",
                )
                .with_source(error)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vended(prefix: &str, expires_ms: Option<&str>) -> StorageCredential {
        let mut config = BTreeMap::from([
            (S3_ACCESS_KEY_ID.to_owned(), "AKIA".to_owned()),
            (S3_SECRET_ACCESS_KEY.to_owned(), "secret".to_owned()),
            (S3_SESSION_TOKEN.to_owned(), "token".to_owned()),
            ("s3.endpoint".to_owned(), "http://rustfs:9000".to_owned()),
        ]);
        if let Some(ms) = expires_ms {
            config.insert(S3_SESSION_TOKEN_EXPIRES_AT_MS.to_owned(), ms.to_owned());
        }
        StorageCredential::from_vended(prefix, &config).unwrap()
    }

    fn far_future_ms() -> String {
        let far = SystemTime::now() + Duration::from_secs(3600);
        far.duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }

    #[test]
    fn vended_properties_become_a_scoped_credential() {
        let credential = vended("s3://warehouse/demo/t1", Some("1792139825000"));

        assert_eq!(credential.prefix.to_string(), "s3://warehouse/demo/t1");
        assert_eq!(
            credential.expires_at,
            Some(UNIX_EPOCH + Duration::from_millis(1_792_139_825_000))
        );
        let CredentialKind::S3 { session_token, .. } = &credential.kind;
        assert_eq!(session_token.as_ref().map(Secret::expose), Some("token"));
        assert!(!format!("{credential:?}").contains("secret\""));
    }

    #[test]
    fn incomplete_or_foreign_credentials_are_rejected() {
        let empty = BTreeMap::new();
        assert_eq!(
            StorageCredential::from_vended("s3://b/t", &empty),
            Err(CredentialError::MissingProperty(S3_ACCESS_KEY_ID))
        );
        assert_eq!(
            StorageCredential::from_vended("gs://b/t", &empty),
            Err(CredentialError::UnsupportedBackend(Backend::Gcs))
        );
        assert!(matches!(
            StorageCredential::from_vended("not a uri", &empty),
            Err(CredentialError::InvalidPrefix(_))
        ));
        let bad_expiry = BTreeMap::from([
            (S3_ACCESS_KEY_ID.to_owned(), "a".to_owned()),
            (S3_SECRET_ACCESS_KEY.to_owned(), "b".to_owned()),
            (EXPIRATION_TIME_MS.to_owned(), "soon".to_owned()),
        ]);
        assert_eq!(
            StorageCredential::from_vended("s3://b/t", &bad_expiry),
            Err(CredentialError::MalformedProperty(EXPIRATION_TIME_MS))
        );
    }

    #[test]
    fn the_cell_selects_the_most_specific_valid_credential() {
        let far = far_future_ms();
        let cell = CredentialCell::new(vec![
            vended("s3://warehouse", Some(&far)),
            vended("s3://warehouse/demo/t1", Some(&far)),
        ]);
        let inside = Location::parse("s3://warehouse/demo/t1/data/f.parquet").unwrap();
        let elsewhere = Location::parse("s3://warehouse/other").unwrap();

        let selected = cell.select(&inside, Duration::from_secs(60)).unwrap();
        assert_eq!(selected.prefix.key(), "demo/t1");
        assert_eq!(
            cell.select(&elsewhere, Duration::ZERO)
                .unwrap()
                .prefix
                .key(),
            ""
        );
    }

    #[test]
    fn the_cell_refuses_uncovered_or_expiring_locations() {
        let cell = CredentialCell::new(vec![vended("s3://warehouse/t1", Some("1000"))]);
        let inside = Location::parse("s3://warehouse/t1/f").unwrap();
        let outside = Location::parse("s3://warehouse/t2/f").unwrap();

        assert!(matches!(
            cell.select(&outside, Duration::ZERO),
            Err(CredentialError::NotCovered(_))
        ));
        assert!(matches!(
            cell.select(&inside, Duration::ZERO),
            Err(CredentialError::Expiring(_))
        ));

        cell.replace(vec![vended("s3://warehouse/t1", Some(&far_future_ms()))]);
        assert!(cell.select(&inside, Duration::from_secs(60)).is_ok());
    }

    #[tokio::test]
    async fn the_cell_serves_file_io_signing_requests() {
        let cell = CredentialCell::new(vec![vended("s3://warehouse/t1", None)]);

        let credential = cell
            .load_credential_with_minimum_validity("s3://warehouse/t1/f", Duration::ZERO)
            .await
            .unwrap();
        let missing = cell
            .load_credential_with_minimum_validity("s3://warehouse/t2/f", Duration::ZERO)
            .await;

        assert_eq!(credential.prefix(), Some("s3://warehouse/t1"));
        assert!(missing.is_err());
        assert!(cell.supports_path("s3a://warehouse/t1/f"));
        assert!(!cell.supports_path("memory://warehouse/t1/f"));
        assert!(
            cell.load_credential_with_minimum_validity("bad", Duration::ZERO)
                .await
                .is_err()
        );
    }
}
