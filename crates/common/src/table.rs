//! Iceberg table facts shared by planning and publication.

/// Table properties whose change invalidates a plan: they decide where and
/// how files are written, or whether the table is encrypted.
///
/// The planner records them in the planning baseline, and the catalog drift
/// check compares them before publication.
pub const PLAN_RELEVANT_PROPERTIES: [&str; 6] = [
    "write.data.path",
    "write.format.default",
    "write.object-storage.enabled",
    "write.object-storage.path",
    "write.metadata.path",
    "encryption.key-id",
];
