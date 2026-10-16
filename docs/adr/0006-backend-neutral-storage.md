# 0006. Backend-neutral storage layer

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2 Object storage layer

## Context

The MVP targets S3, but GCS and Azure must be addable later without touching planning, execution, or publication code.

## Decision

- All storage access goes through `crates/storage`: iceberg-rust `FileIO` (OpenDAL) for table I/O, and a Bergsmith `ObjectStore` trait for listing, stat, deletion, and incomplete uploads.
- Locations are parsed into a typed `Location`; paths are never built by string concatenation.
- Storage profiles and credential sources are configured per catalog connection and worker pool; one adapter module per backend.
- A backend is certified only after it passes the shared contract suite.

## Consequences

- S3 and S3-compatible stores are certified for the MVP; GCS and Azure follow in M9.
