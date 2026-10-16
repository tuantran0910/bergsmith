//! Backend-neutral object storage: typed locations, storage profiles,
//! catalog-vended credentials, and the [`ObjectStore`].
//!
//! Table I/O goes through iceberg-rust `FileIO`, built from a
//! [`StorageProfile`] by [`ObjectStore::open`]. Its `list` and
//! `delete_prefix` treat a path as a directory, never as a raw key prefix;
//! `tests/file_io_smoke.rs` and the contract suite pin that behavior.

pub mod credentials;
pub mod location;
pub mod object_store;
pub mod profile;

pub use credentials::{CredentialCell, CredentialError, CredentialKind, StorageCredential};
pub use location::{Backend, Location, LocationError, Scheme};
pub use object_store::{ObjectEntry, ObjectStore, StorageError};
pub use profile::{
    AzureProfile, BackendCapabilities, GcsProfile, IncompleteUploads, ProfileError, S3Profile,
    StorageProfile, StorageProfiles, capabilities,
};
