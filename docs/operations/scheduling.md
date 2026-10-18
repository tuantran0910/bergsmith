# Scheduling and reconciliation

Construct `scheduling::Scheduler` from the leader's `WorkerService`, a
`SchedulingAccess` provider, and `SchedulerConfig`. Database and epoch come from
the worker service. `CatalogWorkerAccess` implements both worker credential access
and scheduling validation; populate its bindings from each job's original durable
catalog/table identity. Serve `scheduler.worker_service()` and run
`scheduler.run(shutdown)` alongside the tonic endpoint. Calling `reconcile()`
explicitly runs the same startup/periodic repair path.

For each durable plan, materialize its immutable task artifacts and call
`scheduler.register(job_id, offers)` with exactly one offer per planned task.
Each offer binds the authorized pool, capability, artifact URI/digest, and original
resource limits. This registration persists no credentials. Exact retries are
harmless; changed dispatch configuration or retry bounds require a fresh plan/job.
Native artifact content is checked against the plan once during registration.
Registration batches one plan load and credential fetch per job. Refills reconstruct
the registered refs without catalog calls per task; workers verify the original
digest before execution. Exact registration replays use durable state and do not
require artifacts or catalog access, including after the job ends.

Default limits are:

| Limit | Default |
|---|---|
| Admitted jobs globally / per organization / per pool | 8 / 4 / 8 |
| Active managed attempts | 16 |
| Reserved active-attempt memory / CPU | 64 GiB / 32,000 CPU milliseconds |
| Original plan input / estimated duration per job | 64 GiB / 180 seconds |
| Ready cache | Worker service bound; 256 tasks by default |
| Reconciliation interval / database job page | 1 second / 128 jobs |
| External validation/recovery deadline per job | 30 seconds |
| Execution attempts per task | 3, including the original |
| Retry backoff ceiling | 1 second initially, doubling up to 60 seconds |

Configure these through `SchedulerConfig`; worker timings/cache size remain in
`WorkerConfig`. Retry equal jitter chooses a stable delay in the upper half of
the capped exponential window. Retry bounds are persisted per job and deadlines
use database wall time. A restart preserves a future `retry_not_before` exactly.
Changing configuration does not enlarge an existing task's execution envelope or
truncate an already immutable job; an oversized job fails admission explicitly.
The external validation deadline is configurable up to one hour, and source
metadata probes have a fixed concurrency bound of sixteen with deterministic
failure ordering. Increase the deadline for large/high-latency input inventories.

Admission reserves a canonical table, so catalog aliases cannot execute different
jobs against it concurrently. Job slots remain charged until a terminal outcome,
including during unknown publication. Active-attempt memory/CPU and slot budgets
are checked atomically with every managed grant. A full budget returns a transient
same-request failure and never consumes an execution attempt.

Startup keeps new grants closed until it checks leadership, reads retained
publication evidence, repairs execution/cancellation state, and refills the cache.
The bounded cache rotates across pending task identifiers to avoid starvation by
unclaimed tasks in one pool. Its position is expendable; the durable eligible set
and retry timers survive restart. No PostgreSQL notification is required.

Before admission/retry release, the catalog provider compares the exact original
baseline and checks retained planning files plus planned source/delete object
metadata. It never reads table rows. Drift or missing retained planning metadata
ends the job as `ReplanRequired`; a missing/changed input under an unchanged
baseline or revoked authority fails permanently. Transient catalog/storage errors
leave attempts and timers unchanged and close new dispatch until a pass succeeds.
Heartbeats and result/failure acknowledgment recovery remain available.
Healthy periodic validation keeps previously validated dispatch available. Failed
passes, explicit shutdown, and runtime abort close it; workers repeat the same
poll ID after `SchedulerNotReady` / `RetrySameRequest`.

Cancellation and terminal transitions atomically fence unfinished work while
preserving accepted receipts and output inventories. They do not prove that old
processes stopped writing; follow [publication retention](publication.md).
Publication recovery inspects evidence and never resends a commit. An unresolved
publication keeps its barrier and table/job reservations.

BS-014 supplies scheduling and reconciliation library orchestration. Worker
supervisor loops and subprocess enforcement follow in BS-015. Production startup,
policy configuration, and packaging remain later application tasks.
