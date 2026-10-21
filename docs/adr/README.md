# Architecture decision records

Each record captures one decision, its context, and its consequences. Create a
new record from [`template.md`](template.md) with the next number. To change an
accepted decision, add a new record that supersedes it and update the old
record's status; never rewrite an accepted decision.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-coordinator-worker-architecture.md) | Coordinator–worker architecture with a PostgreSQL ledger | Accepted |
| [0002](0002-conservative-conflict-policy.md) | Conservative conflict rejection; compatible rebasing deferred | Accepted |
| [0003](0003-native-executor-on-iceberg-rust.md) | Bergsmith-owned executor on iceberg-rust, without DataFusion | Accepted |
| [0004](0004-replacement-sequence-number.md) | Replacement files take the planning snapshot's sequence number | Accepted |
| [0005](0005-commit-path.md) | Commit through Bergsmith's own CommitTableRequest client | Superseded by 0015 |
| [0006](0006-backend-neutral-storage.md) | Backend-neutral storage layer | Accepted |
| [0007](0007-vended-credentials-and-output-location.md) | Catalog-vended credentials and attempt output location | Accepted |
| [0008](0008-supervisor-subprocess-grpc.md) | Local gRPC between worker supervisor and task subprocess | Accepted |
| [0009](0009-toolchain-and-quality-gates.md) | Stable toolchain and enforced quality gates | Accepted |
| [0010](0010-commit-aligned-scheduling.md) | Commit-aligned job start and per-job duration budget | Accepted |
| [0011](0011-execution-ledger-in-postgresql.md) | Execution ledger schema and access in PostgreSQL | Accepted |
| [0012](0012-worker-service-identity.md) | Worker identity: TLS with pool-scoped bearer tokens | Accepted |
| [0013](0013-bergsmith-owned-catalog-client.md) | Bergsmith owns its catalog HTTP calls | Accepted |
| [0014](0014-object-store-and-development-storage.md) | Object store over FileIO, and RustFS for development | Accepted |
| [0015](0015-commit-adapter-capture.md) | Build commits with a capture-only catalog | Accepted |
| [0016](0016-native-fixtures-and-oracle.md) | Native fixtures and an independent live-row oracle | Accepted |
| [0017](0017-deterministic-planner.md) | A deterministic, metadata-only planner | Accepted |
| [0018](0018-verified-executor-scans.md) | Verify planned identities before native row scans | Accepted; inventory bound superseded by 0028 |
| [0019](0019-bounded-native-rewrites.md) | Bounded native rewrites, receipts and local execution | Accepted; preparation-timeout disposition superseded by 0028 |
| [0020](0020-delete-scope-and-retention.md) | Delete scope and retention during native rewrites | Accepted |
| [0021](0021-per-spec-manifest-dependency-patch.md) | Pin the per-spec manifest dependency patch | Accepted; pinned revision superseded by 0027 |
| [0022](0022-durable-publication.md) | Durable publication and conservative recovery | Accepted; refined by 0029 |
| [0023](0023-worker-sessions-and-durable-pulls.md) | Worker sessions and durable pull assignments | Accepted; failure classification superseded by 0024 |
| [0024](0024-worker-receipt-fencing-and-task-envelopes.md) | Atomic receipt replay, task envelopes, and dispatch reservations | Accepted |
| [0025](0025-durable-admission-and-reconciliation.md) | Durable admission, retry timers, and scheduling reconciliation | Accepted; refined by 0029 |
| [0026](0026-durable-worker-supervision.md) | Durable worker supervision and platform resource enforcement | Accepted; refined by 0030 |
| [0027](0027-delete-applicability-matches-the-native-reader.md) | Delete applicability matches the native reader's metric proof | Accepted |
| [0028](0028-executor-failure-classification-and-preparation-bounds.md) | Executor failure classification and preparation bounds | Accepted |
| [0029](0029-isolated-reconciliation-and-operator-resolution.md) | Isolated reconciliation, publication grace, and operator resolution | Accepted |
| [0030](0030-worker-report-quarantine-and-supervision-recovery.md) | Worker report quarantine and supervision recovery | Accepted |
| [0031](0031-worker-renewal-and-grant-boundaries.md) | Worker renewal and grant boundaries | Accepted |
| [0032](0032-oidc-sessions-and-scoped-authorization.md) | OIDC sessions and scoped authorization | Accepted; refined by 0033 |
| [0033](0033-every-job-names-its-requester.md) | Every job names its requester | Accepted |
| [0034](0034-versioned-browser-resource-api.md) | Versioned browser resources and atomic idempotent submission | Accepted |
| [0035](0035-previews-and-job-commands.md) | Catalog-backed previews and durable job commands | Accepted |
| [0036](0036-reconnectable-job-event-streams.md) | Reconnectable job event streams | Accepted |
| [0037](0037-web-ui-and-generated-client.md) | Web UI and generated API client | Accepted |
| [0038](0038-maintenance-policies-and-schedule-timing.md) | Maintenance policies and schedule timing | Accepted |
| [0039](0039-schedule-occurrence-admission.md) | Schedule occurrence admission and coalescing | Accepted |
