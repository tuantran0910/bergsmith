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
