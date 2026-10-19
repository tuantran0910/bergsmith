# 0019. Bounded native rewrites, receipts and local execution

- Status: Accepted; preparation-timeout disposition superseded by 0028
- Date: 2026-10-17
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Native executor; BS-010.T2–T5
- Extends ADR 0018; supersedes its initial reader defaults and caller-owned I/O deadlines

## Context

Verified input scans need a complete execution path that preserves partition
identity, confines outputs, computes new metrics and reports deterministic
resource failures. Measurements must describe Bergsmith's own executor rather
than the earlier DataFusion spike.

## Decision

- `Executor::execute` owns preparation, footer preflight, native reading, rolling
  writing and receipt assembly. It uses iceberg-rust's delete filter and
  `ParquetWriterBuilder`, `RollingFileWriterBuilder` and `DataFileWriterBuilder`.
  It neither publishes nor deletes source files.
- Resolve the data root through the planning baseline. The supplied prefix must
  equal `<data-location>/bergsmith/<job>/<attempt>`. A runtime-only `FileIO` guard
  permits only new objects below this prefix, rejects deletion operations, and
  shares one semaphore across reads and writes. Typed `Location` values build
  every output path; native partition-directory generation is bypassed.
- Keep each task's original partition spec and values, including old specs.
  Write the pinned current schema by field ID, with unsorted Snappy Parquet output.
  Roll toward the artifact's target (256 MiB by default), close synchronously,
  and never open an output for an empty batch or a fully deleted task.
- Recompute column sizes, counts, null/NaN counts, bounds and split offsets from
  the native writer. Sort receipts canonically and hash them with the protocol
  domain. Verify output row counts, byte counts and the opened-object inventory
  before returning a successful receipt. `rows_read` is the footer-verified
  physical input row count before deletes; `rows_deleted` is its difference from
  output rows. `bytes_read` counts actual storage reads, including preparation
  and footer preflight; it is not the sum of source file sizes.
- Defaults: 1,024-row batches, 32 MiB retained Arrow batch memory, 256 MiB delete
  admission reservation, 128 MiB uncompressed source row groups, 128 MiB per
  ranged request or whole metadata read, 16 MiB Parquet footers, and four shared
  object operations. Output row groups target 8 MiB uncompressed with a 32,768-row
  ceiling, 64 KiB data pages and 256 KiB dictionary pages. There is one active
  data file and no background writer closes. Output limits are 4 GiB and 10,000
  objects per attempt. Storage operations have 30-second deadlines; the attempt
  has a 180-second deadline, including preparation and writer close.
- Preflight verifies actual file sizes and footer row counts before output
  creation. Delete admission reserves eight times the distinct delete files'
  uncompressed size plus 1 KiB per row and equality field (one field for position
  deletes). This bounds admitted delete workloads conservatively; it is not
  native allocator accounting. Native delete loading has no memory-pool API.
- Component budgets and deadlines do not replace the supervisor's total-process
  memory limit. Native blocking CPU work may outlive a cancelled future. BS-015
  owns process termination, cgroups/rlimits, and credential refresh. Failed
  attempts retain partial outputs for the coordinator's inventory and cleanup;
  they never return a successful receipt. Incomplete multipart uploads also
  require the configured bucket lifecycle policy.
- Resource failures are permanent for the immutable task envelope. Only
  explicitly retryable storage/native failures and storage-operation timeouts
  suggest `RetryTask`. Baseline mismatches suggest `ReplanRequired`. Nested native
  error details are omitted from wire failures to avoid exposing credentials.
- `bergsmith-worker local` verifies an externally supplied plan hash, reads only
  its pinned filesystem metadata, and compares the complete planning baseline.
  It runs tasks sequentially with fresh attempt IDs, writes protobuf receipts
  and a summary exclusively into a new directory, and leaves outputs unpublished.
  The distributed local-gRPC context transport remains BS-015.

## Consequences

All eleven spike fixtures pass through the actual writer under independent
row-multiset checks, including shared deletes, nullable keys, schema evolution,
old specs and zero outputs. Resource rejection and rolling metrics have regression
tests. Extended delete-type certification and replacement sequence numbers in
published manifests remain BS-011.

The direct `typetag` dependency is already in the Iceberg dependency graph. It is
needed to implement Iceberg's serializable storage traits. Guards serialize no
credentials or runtime authority; deserialized guards fail closed. Existing
Arrow, Parquet, async-trait and bytes workspace dependencies support streaming,
metrics and quota enforcement.

Process-isolated release measurements and reproducible commands are recorded in
`docs/measurements/bs-010-executor.md`. They establish the acceptance-fixture
envelope, not a Linux isolation or 2 GiB group-size certification.
