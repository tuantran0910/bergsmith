//! Backend-neutral object storage: typed locations, storage profiles, credential
//! sources, and the `ObjectStore` trait.
//!
//! Table I/O goes through iceberg-rust `FileIO`. Its `list` and `delete_prefix`
//! treat a path as a directory, never as a raw key prefix; the behavior this
//! crate relies on is pinned by `tests/file_io_smoke.rs`.
//!
//! Implemented in BS-005.
