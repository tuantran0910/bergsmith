# Support matrix

Status values:

- **Planned:** in the MVP scope, not yet implemented.
- **Certified:** implemented and its acceptance tests pass.
- **Later:** after the MVP (milestone in brackets).
- **Unsupported:** rejected during preview with an explanation.

Certification is scoped to the tested backend or execution path. Native local
rewrite certification does not certify distributed publication or recovery.
Update a row only when its acceptance tests pass (see the project plan §5).

## Table formats and features

| Capability | Status |
|---|---|
| Iceberg format v2 | Planned |
| Iceberg format v3 (deletion vectors, row lineage) | Later (M9) |
| Parquet data and delete files | Planned |
| ORC or Avro data files | Unsupported |
| Branch `main` | Planned |
| Other branches and tags | Unsupported |
| Encrypted tables | Later (M9); rejected at preview |
| Tables with a default sort order | Unsupported until sorting (M9); rejected at preview |
| Partitioned tables, including old partition specs | Planned |
| Schema evolution (renamed and added columns) | Planned |
| Nested types | Planned; pending acceptance tests |

## Maintenance operations

| Operation | Status |
|---|---|
| Whole-file binpack compaction within existing partitions | Certified for native local execution (BS-010, ADRs 0018–0019) and the macOS and unprivileged Linux/local-filesystem distributed recovery fixture with a controlled REST endpoint (BS-016); production catalog/storage combinations remain separately scoped |
| Position delete application | Certified for native local rewrites (BS-011): physical positions, equal-sequence applicability, partition/spec scope, retained shared deletes, and capture-publication manifest checks; distributed acceptance/recovery pending |
| Equality delete application, including null keys | Certified for native local rewrites (BS-011): nullable single/composite keys, duplicate survivors, strict sequence applicability, global equality scope, 50,000-key fixture, and later-delete checks; tables with engine-style column metrics, whose deletes the reader scopes by bounds and null counts (BS-011, ADR 0027); identity-partition columns absent from data files; distributed acceptance/recovery pending |
| Zero-output replacement | Certified for native execution, capture-publication, durable coordinator acceptance/recovery, and worker result RPCs (BS-011–BS-013) |
| Floating-point equality-delete keys | Unsupported; rejected at preview |
| Equality-delete keys on nested or dropped fields | Unsupported; rejected at preview |
| Equality deletes in an all-`void` partition spec that apply to data in other specs | Unsupported; rejected at preview and by workers (ADR 0027) |
| Sorted rewrite | Later (M9) |
| Repartitioning | Unsupported |
| Manifest rewrite | Later (M7) |
| Dangling-delete removal | Later (M7) |
| Snapshot expiration | Later (M7) |
| Orphan file cleanup | Later (M7) |

## Concurrency

| Behavior | Status |
|---|---|
| Conservative conflict rejection (`ReplanRequired`) | Planned; concurrent append, delete, schema, spec, and sort changes rejected by Lakekeeper (BS-007) |
| Durable coordinator publication and recovery | Certified for native files + PostgreSQL + controlled REST crash fixtures (BS-012, ADR 0022); real Lakekeeper send/cancellation-boundary fixtures also pass; macOS and unprivileged Linux process restart and lost-response recovery pass the 100-group distributed gate (BS-016) |
| Worker sessions, durable pulls, lease fencing, and result acknowledgments | Certified for TLS/tonic + PostgreSQL + native receipt fixtures (BS-013, ADRs 0023–0024), and macOS/unprivileged Linux process failure/recovery with 100 groups across three workers (BS-016); runtime, private RPC, subprocess and journal regressions pass (BS-015) |
| Distributed process recovery release gate | Certified on macOS and in an unprivileged Linux aarch64/cgroup v2 container with local files, PostgreSQL, and controlled REST publication (BS-016): killed supervisor with a running native subprocess, lost grant/result/commit responses, coordinator restarts, cancellation after intent, exact row multisets, and one publication; see [acceptance procedure](operations/distributed-acceptance.md) |
| Worker process supervision and credential refresh | Implemented (BS-015, ADR 0026): independent heartbeat/reporting, immutable reporting recovery, cancellation escalation, Linux cgroups and parent-death protection; macOS uses rlimits plus sampled RSS; unprivileged Linux task attachment and parent-death protection pass process acceptance (BS-016); CPU/memory pressure enforcement remains uncertified |
| Durable admission, bounded scheduling, retry timers, and startup/periodic reconciliation | Certified for native Iceberg/TLS/PostgreSQL restart, budget, expiry, retention, and conflict fixtures (BS-014, ADR 0025); coordinator library orchestration, with production packaging following later |
| Commit-aligned job start and per-job duration budget | Planned; conflict rate measured against a synthetic sink ([M1 measurement](measurements/m1-conflict-rate.md)); start alignment and budget admission implemented (BS-009); [native fixture measurements](measurements/bs-010-executor.md) fit the budget, large-group throughput remains unvalidated |
| Compatible rebasing onto concurrent appends | Deferred |

## Catalogs

| Catalog | Status |
|---|---|
| Lakekeeper v0.14.0 | Planned (first certification target); connection, listing, and credential vending verified in BS-005; conditional commits, stale-baseline rejection, snapshot evidence, and idempotent replay verified in BS-006; conflict, lost-response, cancellation-boundary, and output-prefix fixtures pass (BS-007); refuses `write.data.path` |
| Other Iceberg REST catalogs | Later, through compatibility tests |

Connection restrictions and the retention each connection needs are in
[operations/catalog-connections.md](operations/catalog-connections.md).

## Storage

| Backend | Status |
|---|---|
| Amazon S3 with catalog-vended credentials | Planned; contract suite not yet run against AWS |
| S3-compatible stores: RustFS 1.0.1 with STS | Certified (storage contract suite, BS-005) |
| Local filesystem and memory (tests and development) | Certified (storage contract suite, BS-005) |
| Google Cloud Storage | Later (M9) |
| Azure Blob Storage / ADLS Gen2 | Later (M9) |

## Authorization and identity

| Capability | Status |
|---|---|
| OIDC Authorization Code with PKCE, server-side sessions, CSRF | Certified (BS-017, ADR 0032) |
| Scoped role grants (organization, connection, table) | Certified (BS-017): admission and send authorization recheck active grants |
| Maintenance service identity for schedules | Certified (BS-017): `kind = Service` principal with a scoped grant; occurrence/cron execution is BS-019 |
| Mandatory job requester and legacy attribution | Certified (BS-017, ADR 0033): `jobs.created_by` is `NOT NULL`; pre-BS-017 rows are attributed to a revocable per-organization `bergsmith:system` principal |
| Audit log of identity, grant, and maintenance decisions | Certified (BS-017): append-only and secret-free; API exposure is BS-018 |
| Worker pool bearer-token identity and TLS | Implemented (BS-012/013/015, ADRs 0012, 0023, 0026) |

## Deployment

| Target | Status |
|---|---|
| Docker Compose | Planned; development and test services in `deploy/docker/` |
| Kubernetes and Helm | Later (M8) |
