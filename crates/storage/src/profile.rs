//! Storage profiles: how to reach each backend, and what it can do.
//!
//! A profile is configured per catalog connection and worker pool. It names
//! the endpoint and addressing for one backend; credentials come separately,
//! from the catalog. Only the S3 backend (plus memory and the local
//! filesystem for tests and development) is enabled in the MVP. GCS and
//! Azure profiles parse, but building storage for them is refused until
//! their contract suite passes.

use std::collections::BTreeMap;

use crate::location::{Backend, Location};

/// How to reach an S3 or S3-compatible store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Profile {
    /// Endpoint URL for S3-compatible stores; `None` means AWS.
    pub endpoint: Option<String>,
    /// Signing region, such as `us-east-1`.
    pub region: String,
    /// Address buckets as a path (`endpoint/bucket/key`), which most
    /// S3-compatible stores need.
    pub path_style_access: bool,
}

/// How to reach Google Cloud Storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcsProfile {
    /// The project that owns the buckets.
    pub project: Option<String>,
    /// Endpoint URL, for emulators; `None` means Google.
    pub endpoint: Option<String>,
}

/// How to reach Azure Blob Storage or ADLS Gen2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AzureProfile {
    /// The storage account name.
    pub account: String,
    /// Endpoint URL, for emulators; `None` means Azure.
    pub endpoint: Option<String>,
    /// Whether the account has a hierarchical namespace (ADLS Gen2).
    pub hierarchical_namespace: bool,
}

/// One backend's connection settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageProfile {
    /// Amazon S3 or an S3-compatible store.
    S3(S3Profile),
    /// Google Cloud Storage.
    Gcs(GcsProfile),
    /// Azure Blob Storage or ADLS Gen2.
    Azure(AzureProfile),
    /// The local filesystem.
    LocalFs,
    /// In-process memory.
    Memory,
}

impl StorageProfile {
    /// The backend the profile configures.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        match self {
            Self::S3(_) => Backend::S3,
            Self::Gcs(_) => Backend::Gcs,
            Self::Azure(_) => Backend::Azure,
            Self::LocalFs => Backend::LocalFs,
            Self::Memory => Backend::Memory,
        }
    }
}

/// How a backend cleans up uploads left incomplete by a crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncompleteUploads {
    /// The bucket must have a lifecycle rule that aborts incomplete
    /// multipart uploads (S3 `AbortIncompleteMultipartUpload`). Bergsmith's
    /// writers abort their own uploads on failure; the rule covers crashes.
    LifecycleRule,
    /// Writes are not multipart; nothing is left behind.
    NotApplicable,
}

/// What a backend supports, checked when a connection is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// Whether Bergsmith may build storage for the backend in this release.
    pub enabled: bool,
    /// How incomplete uploads are cleaned up.
    pub incomplete_uploads: IncompleteUploads,
    /// Whether vended credentials can be scoped to a location prefix.
    /// Confinement to the attempt output prefix is verified by listing on
    /// every backend regardless.
    pub prefix_scoped_credentials: bool,
}

/// Returns the capabilities of `backend` in this release.
#[must_use]
pub const fn capabilities(backend: Backend) -> BackendCapabilities {
    match backend {
        Backend::S3 => BackendCapabilities {
            enabled: true,
            incomplete_uploads: IncompleteUploads::LifecycleRule,
            prefix_scoped_credentials: true,
        },
        Backend::Gcs | Backend::Azure => BackendCapabilities {
            enabled: false,
            incomplete_uploads: IncompleteUploads::LifecycleRule,
            prefix_scoped_credentials: true,
        },
        Backend::LocalFs | Backend::Memory => BackendCapabilities {
            enabled: true,
            incomplete_uploads: IncompleteUploads::NotApplicable,
            prefix_scoped_credentials: false,
        },
    }
}

/// Why no storage could be built for a location.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    /// No profile is configured for the location's backend.
    #[error("no storage profile is configured for {0:?} locations")]
    NoProfile(Backend),
    /// The backend is not enabled in this release.
    #[error("{0:?} storage is not enabled in this release")]
    BackendNotEnabled(Backend),
    /// The backend needs credentials and none were supplied.
    #[error("{0:?} storage needs catalog-vended credentials")]
    MissingCredentials(Backend),
}

/// The profiles of one catalog connection or worker pool, one per backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageProfiles {
    by_backend: BTreeMap<Backend, StorageProfile>,
}

impl StorageProfiles {
    /// Collects profiles; a later profile for the same backend wins.
    #[must_use]
    pub fn new(profiles: impl IntoIterator<Item = StorageProfile>) -> Self {
        Self {
            by_backend: profiles
                .into_iter()
                .map(|profile| (profile.backend(), profile))
                .collect(),
        }
    }

    /// The configured profiles, one per backend, in backend order.
    ///
    /// The order is stable, so a report of the effective profiles is
    /// deterministic.
    pub fn backends(&self) -> impl Iterator<Item = (&Backend, &StorageProfile)> {
        self.by_backend.iter()
    }

    /// The profile for the backend that serves `location`.
    ///
    /// # Errors
    ///
    /// [`ProfileError::NoProfile`] when the backend has no profile.
    pub fn for_location(&self, location: &Location) -> Result<&StorageProfile, ProfileError> {
        self.by_backend
            .get(&location.backend())
            .ok_or(ProfileError::NoProfile(location.backend()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3() -> StorageProfile {
        StorageProfile::S3(S3Profile {
            endpoint: Some("http://127.0.0.1:59000".to_owned()),
            region: "us-east-1".to_owned(),
            path_style_access: true,
        })
    }

    #[test]
    fn profiles_are_selected_by_the_location_backend() {
        let profiles = StorageProfiles::new([s3(), StorageProfile::Memory]);

        let for_s3a = profiles
            .for_location(&Location::parse("s3a://b/t").unwrap())
            .unwrap();
        let for_gcs = profiles.for_location(&Location::parse("gs://b/t").unwrap());

        assert_eq!(for_s3a, &s3());
        assert_eq!(for_gcs, Err(ProfileError::NoProfile(Backend::Gcs)));
    }

    #[test]
    fn only_tested_backends_are_enabled() {
        assert!(capabilities(Backend::S3).enabled);
        assert!(capabilities(Backend::Memory).enabled);
        assert!(!capabilities(Backend::Gcs).enabled);
        assert!(!capabilities(Backend::Azure).enabled);
        assert_eq!(
            capabilities(Backend::S3).incomplete_uploads,
            IncompleteUploads::LifecycleRule
        );
    }

    #[test]
    fn every_profile_names_its_backend() {
        let gcs = StorageProfile::Gcs(GcsProfile {
            project: None,
            endpoint: None,
        });
        let azure = StorageProfile::Azure(AzureProfile {
            account: "acct".to_owned(),
            endpoint: None,
            hierarchical_namespace: true,
        });
        assert_eq!(gcs.backend(), Backend::Gcs);
        assert_eq!(azure.backend(), Backend::Azure);
        assert_eq!(StorageProfile::LocalFs.backend(), Backend::LocalFs);
    }
}
