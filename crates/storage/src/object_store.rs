//! The Bergsmith object store: `FileIO` built from a profile, plus the
//! inventory and cleanup operations Bergsmith needs, in `Location` terms.
//!
//! Every backend-specific choice lives in [`ObjectStore::open`]; the rest of
//! Bergsmith sees only `ObjectStore` and `FileIO`.

use std::sync::Arc;

use bytes::Bytes;
use futures::TryStreamExt;
use iceberg::io::{FileIO, FileIOBuilder};
use iceberg_storage_opendal::OpenDalStorageFactory;

use crate::credentials::CredentialCell;
use crate::location::{Location, LocationError};
use crate::profile::{ProfileError, StorageProfile, capabilities};

/// One listed object.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectEntry {
    /// Where the object is.
    pub location: Location,
    /// Size in bytes.
    pub size: u64,
    /// Last modification, in milliseconds since the Unix epoch, if known.
    pub last_modified_ms: Option<i64>,
}

/// An error from storage.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Storage cannot be built for the profile.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// The storage operation failed.
    #[error("storage operation failed")]
    Io(#[from] iceberg::Error),
    /// The backend listed a path that is not a valid location.
    #[error("storage listed an invalid location")]
    InvalidListing(#[source] LocationError),
}

/// `FileIO` for one backend, with location-typed helpers.
#[derive(Clone, Debug)]
pub struct ObjectStore {
    file_io: FileIO,
}

impl ObjectStore {
    /// Builds storage for `profile`.
    ///
    /// Credentials come only from `credentials`, never from the
    /// environment: ambient configuration and instance metadata are
    /// disabled, so a missing credential fails instead of falling back.
    ///
    /// # Errors
    ///
    /// [`ProfileError::BackendNotEnabled`] for backends not enabled in this
    /// release, and [`ProfileError::MissingCredentials`] for an S3 profile
    /// without credentials.
    pub fn open(
        profile: &StorageProfile,
        credentials: Option<CredentialCell>,
    ) -> Result<Self, StorageError> {
        let backend = profile.backend();
        if !capabilities(backend).enabled {
            return Err(ProfileError::BackendNotEnabled(backend).into());
        }
        let file_io = match profile {
            StorageProfile::S3(s3) => {
                let credentials = credentials.ok_or(ProfileError::MissingCredentials(backend))?;
                let mut builder = FileIOBuilder::new(Arc::new(OpenDalStorageFactory::s3()))
                    .with_prop("s3.region", &s3.region)
                    .with_prop("client.region", &s3.region)
                    .with_prop("s3.path-style-access", s3.path_style_access)
                    .with_prop("s3.disable-config-load", true)
                    .with_prop("s3.disable-ec2-metadata", true);
                if let Some(endpoint) = &s3.endpoint {
                    builder = builder.with_prop("s3.endpoint", endpoint);
                }
                builder
                    .with_credential_provider(Arc::new(credentials))
                    .build()
            }
            StorageProfile::LocalFs => {
                FileIOBuilder::new(Arc::new(OpenDalStorageFactory::fs())).build()
            }
            StorageProfile::Memory => {
                FileIOBuilder::new(Arc::new(OpenDalStorageFactory::memory())).build()
            }
            StorageProfile::Gcs(_) | StorageProfile::Azure(_) => {
                return Err(ProfileError::BackendNotEnabled(backend).into());
            }
        };
        Ok(Self { file_io })
    }

    /// The underlying `FileIO`, for Iceberg readers and writers.
    #[must_use]
    pub fn file_io(&self) -> &FileIO {
        &self.file_io
    }

    /// Lists every object under `prefix`, recursively, sorted by location.
    /// Directory placeholders are omitted.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] when listing fails and
    /// [`StorageError::InvalidListing`] when the backend returns a path that
    /// does not parse.
    pub async fn list(&self, prefix: &Location) -> Result<Vec<ObjectEntry>, StorageError> {
        let mut entries: Vec<ObjectEntry> = self
            .file_io
            .list(prefix.to_dir_uri(), true)
            .await?
            .try_filter(|entry| std::future::ready(!entry.is_dir))
            .map_err(StorageError::Io)
            .and_then(|entry| {
                std::future::ready(
                    Location::parse_listed(&entry.path)
                        .map(|location| ObjectEntry {
                            location,
                            size: entry.size,
                            last_modified_ms: entry.last_modified_ms,
                        })
                        .map_err(StorageError::InvalidListing),
                )
            })
            .try_collect()
            .await?;
        entries.sort();
        Ok(entries)
    }

    /// Whether an object exists at `location`.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] when the check fails.
    pub async fn exists(&self, location: &Location) -> Result<bool, StorageError> {
        Ok(self.file_io.exists(location.to_string()).await?)
    }

    /// Reads a whole object. For small objects such as task artifacts.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] when the object is missing or the read fails.
    pub async fn read(&self, location: &Location) -> Result<Bytes, StorageError> {
        Ok(self.file_io.new_input(location.to_string())?.read().await?)
    }

    /// Writes a whole object, replacing any existing one.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] when the write fails.
    pub async fn write(&self, location: &Location, bytes: Bytes) -> Result<(), StorageError> {
        Ok(self
            .file_io
            .new_output(location.to_string())?
            .write(bytes)
            .await?)
    }

    /// Deletes one object. Deleting a missing object succeeds.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] when the delete fails.
    pub async fn delete(&self, location: &Location) -> Result<(), StorageError> {
        Ok(self.file_io.delete(location.to_string()).await?)
    }

    /// Deletes every object under `prefix`, comparing whole segments, so
    /// `.../job` never touches `.../job-other`.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] when a delete fails.
    pub async fn delete_prefix(&self, prefix: &Location) -> Result<(), StorageError> {
        Ok(self.file_io.delete_prefix(prefix.to_dir_uri()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{AzureProfile, GcsProfile, S3Profile};

    #[test]
    fn disabled_backends_and_missing_credentials_are_refused() {
        let gcs = StorageProfile::Gcs(GcsProfile {
            project: None,
            endpoint: None,
        });
        let azure = StorageProfile::Azure(AzureProfile {
            account: "a".to_owned(),
            endpoint: None,
            hierarchical_namespace: false,
        });
        let s3 = StorageProfile::S3(S3Profile {
            endpoint: None,
            region: "us-east-1".to_owned(),
            path_style_access: false,
        });

        for (profile, expected) in [
            (&gcs, "not enabled"),
            (&azure, "not enabled"),
            (&s3, "needs catalog-vended credentials"),
        ] {
            let error = ObjectStore::open(profile, None).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
        assert!(ObjectStore::open(&s3, Some(CredentialCell::default())).is_ok());
    }
}
