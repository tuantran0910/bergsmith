# Support matrix

Status values:

- **Planned:** in the MVP scope, not yet implemented.
- **Certified:** implemented and its acceptance tests pass.
- **Later:** after the MVP (milestone in brackets).
- **Unsupported:** rejected during preview with an explanation.

Nothing is certified yet. Update a row only when its acceptance tests pass
(see `.private/BERGSMITH_PROJECT_PLAN.md` §5).

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
| Whole-file binpack compaction within existing partitions | Planned; deterministic planner implemented (BS-009, ADR 0017), executor pending |
| Position delete application | Planned |
| Equality delete application, including null keys | Planned |
| Floating-point equality-delete keys | Unsupported; rejected at preview |
| Equality-delete keys on nested or dropped fields | Unsupported; rejected at preview |
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
| Commit-aligned job start and per-job duration budget | Planned; conflict rate measured against a synthetic sink ([M1 measurement](measurements/m1-conflict-rate.md)); start alignment and budget admission implemented (BS-009), throughput not yet measured |
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

## Deployment

| Target | Status |
|---|---|
| Docker Compose | Planned; development and test services in `deploy/docker/` |
| Kubernetes and Helm | Later (M8) |
