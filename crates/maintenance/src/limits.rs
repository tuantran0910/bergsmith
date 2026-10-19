//! Explicit, per-attempt execution budgets.

use serde::{Deserialize, Serialize};

use crate::ExecutorError;

/// Budgets for one immutable task attempt. No sorting or spilling is supported.
///
/// Delete memory is a conservative admission reservation computed from Parquet
/// uncompressed sizes and predicate nodes. Batch memory is checked on decoded
/// batches. These are component budgets; the supervisor must separately enforce
/// the total process memory limit (BS-015).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionLimits {
    /// Maximum rows per decoded batch.
    pub batch_rows: usize,
    /// Maximum retained Arrow bytes for one decoded batch.
    pub batch_memory_bytes: u64,
    /// Maximum sum of conservative reservations for distinct delete files.
    pub delete_memory_bytes: u64,
    /// Maximum uncompressed size of a source Parquet row group, before decoding.
    pub decode_row_group_bytes: u64,
    /// Maximum bytes in a whole metadata read or one ranged object request.
    pub read_request_bytes: u64,
    /// Uncompressed bytes per output row group; bounds writer buffering.
    pub writer_row_group_bytes: usize,
    /// Maximum Parquet footer bytes for a source or delete file.
    pub footer_bytes: u64,
    /// Maximum simultaneous storage operations, shared by readers and writer.
    pub object_requests: u32,
    /// Maximum physical bytes submitted to storage across all output files.
    pub output_bytes: u64,
    /// Maximum number of output objects opened.
    pub output_files: usize,
    /// Per-storage-operation deadline, including waiting for an I/O permit.
    pub io_timeout_ms: u64,
    /// Whole attempt deadline, including preparation and output close.
    pub duration_ms: u64,
    /// The most live manifest entries preparation reads; it bounds inventory
    /// memory. Unset uses [`PlannerConfig`]'s default `max_manifest_entries`.
    /// Set it to at least the plan's recorded bound, or preparation of a
    /// table the planner accepted fails with an inventory error. Local
    /// execution sets the plan's bound when this is unset.
    ///
    /// [`PlannerConfig`]: crate::PlannerConfig
    pub manifest_entries: Option<u64>,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            batch_rows: 1024,
            batch_memory_bytes: 32 * 1024 * 1024,
            delete_memory_bytes: 256 * 1024 * 1024,
            decode_row_group_bytes: 128 * 1024 * 1024,
            read_request_bytes: 128 * 1024 * 1024,
            writer_row_group_bytes: 8 * 1024 * 1024,
            footer_bytes: 16 * 1024 * 1024,
            object_requests: 4,
            output_bytes: 4 * 1024 * 1024 * 1024,
            output_files: 10_000,
            io_timeout_ms: 30_000,
            duration_ms: 180_000,
            manifest_entries: None,
        }
    }
}

impl ExecutionLimits {
    /// Refuses zero budgets and values the native APIs cannot represent safely.
    ///
    /// # Errors
    ///
    /// [`ExecutorError::InvalidLimits`] if any budget is zero, or concurrency
    /// exceeds 64. Limits are validated before storage access or output creation.
    pub fn validate(&self) -> Result<(), ExecutorError> {
        if self.batch_rows == 0
            || self.batch_memory_bytes == 0
            || self.delete_memory_bytes == 0
            || self.decode_row_group_bytes == 0
            || self.read_request_bytes == 0
            || self.writer_row_group_bytes == 0
            || self.footer_bytes == 0
            || self.object_requests == 0
            || self.object_requests > 64
            || self.output_bytes == 0
            || self.output_files == 0
            || self.io_timeout_ms == 0
            || self.duration_ms == 0
            || self.manifest_entries == Some(0)
            || self.batch_rows > i32::MAX as usize
            || self.writer_row_group_bytes > i32::MAX as usize
        {
            return Err(ExecutorError::InvalidLimits);
        }
        Ok(())
    }

    /// The effective live manifest entry bound of preparation.
    #[must_use]
    pub fn manifest_entries(&self) -> u64 {
        self.manifest_entries
            .unwrap_or_else(|| crate::PlannerConfig::default().max_manifest_entries)
    }
}

/// The deterministic resource budget an attempt exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceLimit {
    /// Retained decoded Arrow buffers.
    BatchMemory,
    /// Delete predicate/vector admission reservation.
    DeleteMemory,
    /// Uncompressed source row group.
    DecodeRowGroup,
    /// Allocation for a whole metadata read or ranged object request.
    ReadBuffer,
    /// Parquet footer allocation.
    Footer,
    /// Physical output storage bytes.
    OutputBytes,
    /// Output object inventory.
    OutputFiles,
    /// Whole attempt wall-clock duration.
    Duration,
}

impl ResourceLimit {
    /// Stable failure reason reported to the coordinator.
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::BatchMemory => "BatchMemoryLimitExceeded",
            Self::DeleteMemory => "DeleteMemoryLimitExceeded",
            Self::DecodeRowGroup => "DecodeRowGroupLimitExceeded",
            Self::ReadBuffer => "ReadBufferLimitExceeded",
            Self::Footer => "FooterLimitExceeded",
            Self::OutputBytes => "OutputBytesLimitExceeded",
            Self::OutputFiles => "OutputFilesLimitExceeded",
            Self::Duration => "TaskDurationExceeded",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_zero_budget_is_rejected() {
        let cases = [
            ExecutionLimits {
                batch_rows: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                batch_memory_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                delete_memory_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                decode_row_group_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                read_request_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                writer_row_group_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                footer_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                object_requests: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                object_requests: 65,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                output_bytes: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                output_files: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                io_timeout_ms: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                duration_ms: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                manifest_entries: Some(0),
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                batch_rows: usize::MAX,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                writer_row_group_bytes: usize::MAX,
                ..ExecutionLimits::default()
            },
        ];
        assert!(ExecutionLimits::default().validate().is_ok());
        for limits in cases {
            assert!(matches!(
                limits.validate(),
                Err(ExecutorError::InvalidLimits)
            ));
        }
    }

    #[test]
    fn resource_failure_codes_are_stable_and_distinct() {
        let limits = [
            ResourceLimit::BatchMemory,
            ResourceLimit::DeleteMemory,
            ResourceLimit::DecodeRowGroup,
            ResourceLimit::ReadBuffer,
            ResourceLimit::Footer,
            ResourceLimit::OutputBytes,
            ResourceLimit::OutputFiles,
            ResourceLimit::Duration,
        ];
        assert_eq!(
            limits.map(ResourceLimit::reason_code),
            [
                "BatchMemoryLimitExceeded",
                "DeleteMemoryLimitExceeded",
                "DecodeRowGroupLimitExceeded",
                "ReadBufferLimitExceeded",
                "FooterLimitExceeded",
                "OutputBytesLimitExceeded",
                "OutputFilesLimitExceeded",
                "TaskDurationExceeded"
            ]
        );
    }
}
