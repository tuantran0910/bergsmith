# Operator guide

Read these in order to install, connect, and run Bergsmith. Each page owns its
topic; this index only says where to look.

| Task | Page |
|---|---|
| Install: migrate, configure, and start the coordinator; database TLS | [serving.md](serving.md) |
| Sign-in, roles, and the first administrator | [identity.md](identity.md) |
| Connect a catalog, its storage, a worker pool, and tables, end to end | [setup.md](setup.md) |
| Catalog requirements and the retention and cleanup you must configure | [catalog-connections.md](catalog-connections.md) |
| Run workers: configuration, isolation, journal retention | [workers.md](workers.md) |
| Policies, schedules, windows, and budgets | [scheduling.md](scheduling.md) |
| Publication outcomes and unknown-outcome resolution | [publication.md](publication.md) |
| Upgrades and recovery runbooks | [recovery.md](recovery.md) |
| Metrics, probes, structured logs, and worker health | [observability.md](observability.md) |
| Browser API and UI | [api.md](api.md), [web-ui.md](web-ui.md) |
| Unpublished local execution and the distributed acceptance suite | [local-execution.md](local-execution.md), [distributed-acceptance.md](distributed-acceptance.md) |

What is supported, and what has passed acceptance, is in the
[support matrix](../support-matrix.md). Design decisions are in the
[ADRs](../adr/README.md).

## Retention and cleanup at a glance

Bergsmith never deletes source files, snapshots, or abandoned outputs. Three
retention settings belong to the operator:

| What | Where it is configured | Requirement |
|---|---|---|
| Table snapshots, source files, and orphan cleanup | The catalog and its maintenance tooling | Longer than the execution budget plus the longest unresolved publication you tolerate; exclude `<data-location>/bergsmith/` from orphan cleanup while jobs are unresolved ([catalog-connections.md](catalog-connections.md#retention-you-must-configure)) |
| Incomplete multipart uploads | The bucket lifecycle | An `AbortIncompleteMultipartUpload` rule |
| Acknowledged worker journal records | `journal_retention_hours` in each worker's configuration | 30 days by default; unacknowledged and quarantined records are never removed ([workers.md](workers.md#journal-retention)) |

Abandoned attempt outputs (failed, cancelled, expired, or conflicted attempts)
stay under their attempt prefix. Reclaim them with your orphan cleaner once no
job that wrote them is `Committing` or `CommitOutcomeUnknown`.

## Known gaps in this release

- Production catalog and storage certification is pending: Lakekeeper v0.14.0
  and Amazon S3 are not certified, and the Docker Compose deployment awaits
  the production acceptance run ([support matrix](../support-matrix.md)).
- Connections with an external `credential_ref` fail until the runtime
  resolves credential references ([api.md](api.md)).
- Broader maintenance (snapshot expiration, orphan cleanup, manifest rewrite,
  dangling-delete removal) and notifications are not available.
- Kubernetes and Helm are planned for M8.
