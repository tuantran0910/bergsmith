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
| Send grace before periodic recovery of an authorized `Committing` job | 120 seconds |
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

Before admission and before releasing a due retry, the catalog provider compares
the exact original baseline and checks retained planning files plus planned
source/delete object metadata. It never reads table rows. These are the only
times a job is validated: an admitted job without due retries is not revalidated
each pass, and an unadmitted job waits without validation while its table or job
budgets are full. Expired attempts are fenced every pass without validation.
Drift or missing retained planning metadata ends the job as `ReplanRequired`; a
missing/changed input under an unchanged baseline or a catalog 403 fails
permanently. A catalog 401/419 or token-endpoint refusal is a coordinator
credential problem and stays transient; the client discards the rejected token.
A stop applies only at the job revision it was decided on.

Failures are isolated per job: a transient catalog/storage error, timeout,
missing binding, or invalid stored plan skips that job (logged with
`job_id` and `reason_code`), leaving its attempts and timers unchanged, and the
pass continues for every other job. Only lost leadership or database failure
fails the pass and closes new dispatch. Heartbeats and result/failure
acknowledgment recovery remain available. Failed passes, explicit shutdown, and
runtime abort close dispatch; workers repeat the same poll ID after
`SchedulerNotReady` / `RetrySameRequest`.

The first pass recovers every incomplete publication. Later passes skip a
`Committing` job whose send was authorized within `publication_send_grace`, so a
live publisher records its own response. Keep the grace above the catalog
connect plus request timeout. Unknown outcomes keep their barrier and job slots;
`Scheduler::unresolved_publications` and a warning log report their count (see
[resolving an unknown outcome](publication.md#resolving-an-unknown-outcome)).

Cancellation and terminal transitions atomically fence unfinished work while
preserving accepted receipts and output inventories. They do not prove that old
processes stopped writing; follow [publication retention](publication.md).
Publication recovery inspects evidence and never resends a commit. An unresolved
publication keeps its barrier and table/job reservations.

## Schedules, windows, and run budgets

A maintenance schedule runs one policy against one table. Each enabled schedule
has a durable `next_due_at`; the coordinator's `ScheduleRunner` admits every due
schedule in its own transaction, fenced by the coordinator epoch. An occurrence
row keyed by `(organization, schedule, first due instant)` is written before the
job, so a replay cannot admit a second job. Missed occurrences coalesce into one
evaluation, and an occurrence that overlaps a nonterminal job on the same table
is recorded as `Coalesced` with reason `TableBusy` instead of starting a second
run.

An optional schedule window bounds when a run may start. It is a daily
local-time interval in the schedule's IANA zone: the start minute is inclusive,
the end minute exclusive, and a start later than the end wraps past midnight
(for example `22:00`–`02:00`). Admission checks the instant at which the job
would be created, not the occurrence's nominal time. An occurrence evaluated
outside the window is recorded as one `Skipped` row with reason `OutsideWindow`
covering the due span up to the evaluation instant, and the cursor jumps to the
first occurrence at or after the next opening; the run itself is dropped, not
deferred. A cron more frequent than its window therefore does not write one
skipped row per tick. The skip is logged at debug level and is not audited,
because it is routine scheduling; only a disabled policy or an ineligible
service identity is an operator denial. A run that starts inside the window may
finish after it closes. The window does not change occurrence identity,
deduplication, or coalescing, and it is not rechecked at publication.

Checks are ordered `PolicyDisabled`, `ServiceIdentityIneligible`,
`OutsideWindow`, then `TableBusy`: a run that may not start never folds into
another job's occurrence. A window uses the zone's wall clock, so a
spring-forward gap shortens it and a fall-back overlap lengthens it; the cron
engine still decides which instants exist. A repeated fall-back start is two
candidate openings: the earlier is preferred, but if it is already past, the
later reading still opens the window. That differs from cron's deduplication to
the earlier instant, and it keeps a valid run later that morning.

Window eligibility samples the coordinator's clock **after** the schedule and
state row locks, so a pass that waited on a lock decides with the instant it
actually has, not the pass start. Arming and window-edit re-arming use the
database clock; a small difference remains, but the window is a start guard and
publication does not depend on it.

Changing a window recomputes a future cursor to the next cron occurrence after
the decision instant, because a skip under the old window may have advanced it
past newly eligible runs. An overdue cursor is preserved so a due run is still
evaluated under the new window, and re-sending the same window changes nothing.
The update samples its clock after locking the schedule row, and the next
cursor is also after the **latest evaluated occurrence** for the schedule
(`max(coalesced_until, scheduled_for)`). That bound makes the recomputation
independent of clock skew: admission uses the coordinator clock while window
edits, timing changes, and re-enables use the database clock, and a database
clock slightly behind a recorded occurrence can no longer rewind the cursor onto
it and leave every later admission failing the uniqueness constraint. The same
bound applies when a timing change or re-enable arms the cursor.

A policy's `max_input_bytes` is its per-run budget: the most input one run may
rewrite. It defaults to 100 GiB, is bounded at 1 MiB–100 GiB, and must be at
least the policy's target file size. `PlannerConfig::for_policy` applies it when
a scheduled job is planned, replacing the target and minimum input-file count
and capping the job and group limits. It takes the coordinator's global
admission cap as an argument, so the effective budget is the smallest of the
policy, base planner, and admission values and the group limit never exceeds the
job limit. The budget is recorded in the immutable plan; tightening a policy
later does not invalidate an already planned run. Disabling a policy stops
future admission and also blocks a scheduled job from publishing, while
disabling only a schedule leaves an existing job alone.
