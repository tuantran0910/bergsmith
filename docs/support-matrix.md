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
| Worker sessions, durable pulls, lease fencing, and result acknowledgments | Certified for TLS/tonic + PostgreSQL + native receipt fixtures (BS-013, ADRs 0023–0024), and macOS/unprivileged Linux process failure/recovery with 100 groups across three workers (BS-016); runtime, private RPC, subprocess and journal regressions pass (BS-015); acknowledged journal records expire after a configurable retention with crash-safe removal (BS-020.T3, ADR 0043) |
| Distributed process recovery release gate | Certified on macOS and in an unprivileged Linux aarch64/cgroup v2 container with local files, PostgreSQL, and controlled REST publication (BS-016): killed supervisor with a running native subprocess, lost grant/result/commit responses, coordinator restarts, cancellation after intent, exact row multisets, and one publication; see [acceptance procedure](operations/distributed-acceptance.md) |
| Worker process supervision and credential refresh | Implemented (BS-015, ADR 0026): independent heartbeat/reporting, immutable reporting recovery, cancellation escalation, Linux cgroups and parent-death protection; macOS uses rlimits plus sampled RSS; unprivileged Linux task attachment and parent-death protection pass process acceptance (BS-016); CPU/memory pressure enforcement remains uncertified |
| Durable admission, bounded scheduling, retry timers, and startup/periodic reconciliation | Certified for native Iceberg/TLS/PostgreSQL restart, budget, expiry, retention, and conflict fixtures (BS-014, ADR 0025); the BS-020.T1 `serve` runtime composes scheduling, planning, publication, HTTP, and worker RPC in one process; production hardening remains T4/T6 |
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
| Effective session permissions | Implemented (BS-023.T3, proposed ADR 0054): `GET /auth/session` expands the caller's active grants into deduplicated, sorted `{permission, scope}` pairs from the same role table authorization uses, recomputed on every read. The browser gates navigation and actions on those pairs instead of role names; the table page hides preview and submission without `jobs:submit`, and the job page hides cancellation and retry without `jobs:cancel` or `jobs:retry`. Table coverage matches the server: an organization grant, the table's own grant, or a grant on any connection the table is named on. The API remains the authority and re-checks the ledger. Route, unit, and journey tests cover scoped expansion, deduplication, revocation without a new sign-in, organization coverage, a Viewer with no actions, grants covering another table or connection, and alias connection coverage |
| Scoped role grants (organization, connection, table) | Certified (BS-017): admission and send authorization recheck active grants. BS-023.T2 adds browser listing, creation, and logical revocation under the grant's own scope |
| Grant administration API | Implemented (BS-023.T2, proposed ADR 0053): `GET`, `POST`, and `DELETE /api/v1/grants` list, create, and revoke grants under `grants:manage` over the grant's scope. A connection administrator is limited to that connection and its tables; an organization administrator sees every grant. Listing is keyset-paginated with the filters bound into the cursor, revoked rows are kept and never listed, and an identical active grant is 409 `grant_already_exists`. Revocation is logical and audited in the same transaction. The last active organization-wide Administrator grant cannot be revoked: scoped administrators do not replace it, and the guard takes the organization lifecycle lock before authorization, so it serializes with deactivation and concurrent revocations. Creation takes the same lock before authorization, so a grant cannot be inserted after a revocation commits and restore the access it just removed; a paused-revocation regression covers the interleaving. Route tests cover scope limits, duplicate and invalid grants, paging, audit rows, the last-administrator guard, and a revocation ordered behind the lifecycle lock |
| Maintenance service identity for schedules | Certified (BS-017): `kind = Service` principal with a scoped grant; BS-019.T2 rechecks the grant at occurrence admission, job admission, and publication |
| Mandatory job requester and legacy attribution | Certified (BS-017, ADR 0033): `jobs.created_by` is `NOT NULL`; pre-BS-017 rows are attributed to a revocable per-organization `bergsmith:system` principal |
| Audit log of identity, grant, and maintenance decisions | Certified (BS-017): append-only and secret-free; read-only API exposure by BS-018.T1; scheduled admissions and denials are audited by BS-019.T2 |
| Worker pool bearer-token identity and TLS | Implemented (BS-012/013/015, ADRs 0012, 0023, 0026) |
| Coordinator PostgreSQL TLS | Implemented (BS-020.T6, ADR 0042): rustls with libpq `sslmode`/`sslrootcert`/`sslcert`/`sslkey` URL parameters for `serve`, `migrate`, and administration commands; `require`, `verify-full` with a trusted CA, and rejection of an untrusted CA pass against the TLS-enabled test PostgreSQL; a startup warning flags modes weaker than `verify-full` |

## Browser API and UI

| Capability | Status |
|---|---|
| Catalog connection create, edit, enable, and disable API | Implemented (BS-022.T1, proposed ADR 0047): PostgreSQL route, audit rollback, revision, scoped-alias admission, lock-race, and disabled-connection publication recovery tests pass; discovery and browser pages are implemented in BS-022.T4–T5 |
| Catalog connection capability check | Implemented (BS-022.T2, proposed ADR 0048): `POST /api/v1/catalogs/{id}/check` reads `GET /v1/config`, records the advertised endpoints, the credentials endpoint, and the idempotency-key lifetime with the check time, and reports every requirement as met, missing, or unknown. The answer is bound to the routing it describes and is not recorded or shown after a routing edit; the catalog answer is bounded to 64 KiB and the coordinator runs at most 8 checks at once and 2 per requester. Probe, ledger, and route tests pass, including a chunked oversized answer and capacity exhaustion. Vended credentials stay unknown until a table is loaded. BS-022.T5 also checks unsaved drafts under the same limits and organization administration permission, without creating a connection or saved report |
| Catalog connection pages | Implemented (BS-022.T5, proposed ADR 0051): authorized connection list/detail, capability diagnostics before saving a draft, revision-checked settings and routing, read-only storage profiles and unresolved-publication reports, cursor-based namespace/table discovery, single-table/namespace/name registration, and confirmed alias removal by observed revision. Session-fenced mutations are never retried automatically; an uncertain create requires inspecting the list. Guided setup still requires an unauthenticated catalog; credential reference resolution is deferred |
| Principal administration and deactivation | Implemented (BS-023.T1, proposed ADR 0052): administrators list the organization's users and service accounts, provision a user for an issuer and subject, and deactivate or reactivate a principal under organization-wide `grants:manage`. Deactivation keeps the row, its grants, and its history, ends its sessions in the same transaction, refuses sign-in, and stops every admission and publication check from applying its grants while leaving running jobs untouched; a repeated command writes nothing. The last active administrator cannot be deactivated, and only organization-scoped Administrator grants count as a replacement; the organization-scoped transaction lock is taken before the actor is authorized, so concurrent changes cannot each act on the state the other replaces. Sign-in holds a share lock on the principal from the active check through session insertion, so a racing deactivation either waits and revokes the new session or refuses the sign-in. Sign-in keeps the display name in step with the ID token. Route and persistence tests cover permissions, deactivation of sessions, admission, and read access, reactivation, the last-administrator guard, scoped replacements, concurrent self-reactivation and mutual deactivation, a racing sign-in, transactional denial audit, audit rows, and paging |
| Per-connection operating settings | Implemented (BS-022.T3, proposed ADR 0049): the execution budget and the unresolved-publication age are stored with the connection, revision-checked and audited. The budget bounds the whole job: the plan carries a deadline, every attempt, retry, and lease renewal is capped at it, and a spent budget ends the job with `ExecutionBudgetExceeded` without disturbing an authorized publication. The detail reports overdue unresolved publications, a paused connection admits no new maintenance work and needs a grant on a connection that allows it, and the effective storage profile is reported read-only from the coordinator configuration without an endpoint's user information, query, or fragment |
| Catalog discovery and table registration | Implemented (BS-022.T4, proposed ADR 0050): a connection's namespaces and tables are browsed through the catalog one page at a time, continuing on the catalog's own token, with unaddressable identifiers counted and a page over 1000 items or 1 MiB of names refused, and at most 4 discovery operations at once; one table or a whole namespace is registered by loading every table before anything is written, recording the aliases and their audit in one transaction, refusing more than 100 tables or a name the API cannot address, and refusing a catalog that answers with another namespace. Registering a name twice answers with the identity the first request recorded. A name is removed by the revision the browse reported, or by the alias revision the table resource carries when the catalog no longer lists it, without deleting the table, and a table with nonterminal jobs keeps all of its names. Route tests cover permissions, idempotency, continuation, paged namespace registration, a catalog-dropped name, the namespace bound, unaddressable names, atomic rollback, audit rollback, an unreadable or replaced table, stale removal, the in-use guard, and catalog failures |
| `/api/v1` catalog, table, job, and audit reads; idempotent job submission | Implemented as a session-authenticated router (BS-018.T1, ADR 0034); scoped reads, cursor pagination, CSRF, concurrent idempotency, and audit rollback pass PostgreSQL route tests; BS-020.T1 serves it with `/auth` and the UI on one origin |
| Table previews and cancellation/fresh-plan retry commands | Implemented (BS-018.T2, ADR 0035) for unauthenticated catalog connections: previews resolve a grant-scoped alias, verify the canonical `table-uuid`, and are bounded per response and per process; commands follow the cancellation contract and never reuse a plan. Connections with an external `credential_ref` fail until the runtime resolves credential references |
| Durable SSE job events | Implemented (BS-018.T3, ADR 0036): per-job sequences as event IDs, `Last-Event-ID`/`after` resume without gaps or duplicates, notification wake-ups with a polling fallback, session and grant rechecks, and bounded capacity pass PostgreSQL route tests; BS-020.T1 serves it through the coordinator `serve` runtime |
| Generated API client and manual-maintenance UI journeys | Implemented (BS-018.T4, ADR 0037): browse, preview, submit, live progress, cancellation, fresh-plan retry, and audit pass journey tests against a contract-typed fake coordinator; BS-020.T1 serves the build from the same origin, so the browser acceptance run is available for T4 |
| Metrics, readiness, structured logs, and worker health | Implemented (BS-020.T2, ADR 0044): ledger-derived Prometheus metrics with a cached, time-bounded snapshot, worker RPC latency histograms, `/readyz`, JSON logs with span context, and `GET /api/v1/workers` with a Workers page pass PostgreSQL, route, and journey tests; production dashboards and alerting acceptance remain T4 |
| Schedule and policy configuration | Implemented (BS-019.T1, ADR 0038): five-field cron with IANA time zones and the plan's DST rules, optimistic revisions, audited edits, and a service-identity eligibility check pass unit and PostgreSQL route tests; windows and budgets are BS-019.T3 |
| Schedule occurrence admission and coalescing | Implemented (BS-019.T2, ADR 0039): occurrence identity is unique per schedule and first due instant, missed occurrences coalesce into one evaluation, an active job on the table coalesces later evaluations, disabled schedules stop admission without backfilling, existing schedules are armed on the first post-upgrade pass, a bounded rotating due scan prevents one failing schedule from starving others, and disabled policies or revoked service grants skip admission and block publication; duplicate, downtime, overlap, disabled, time-zone, DST, upgrade, and starvation scenarios pass PostgreSQL and coordinator-runner tests |

See the [API contract and current limits](operations/api.md) and the [web UI](operations/web-ui.md).

## Deployment

| Target | Status |
|---|---|
| Docker Compose | Implemented for development (BS-020.T1): `deploy/docker/compose.app.yaml` with digest-pinned base images, coordinator/worker/web image targets, separate migration and bootstrap jobs, an nginx same-origin web container, PostgreSQL, RustFS, Lakekeeper, Keycloak, and TLS certificate init; production acceptance remains T4 |
| Kubernetes and Helm | Later (M8) |
