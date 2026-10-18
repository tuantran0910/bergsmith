# 0025. Durable admission, retry timers, and scheduling reconciliation

- Status: Accepted
- Date: 2026-10-18
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-014.T1–T4

## Context

The worker service can persist grants and recover acknowledgments, but its ready
cache cannot be the source of task eligibility. Restarting must reconstruct safe
dispatch fields, repair worker/session loss, and preserve retry deadlines. Catalog
aliases must share table ownership, including while publication remains unknown.

## Decision

- Persist immutable, credential-free dispatch specifications for every task of a
  registered plan. Registration is atomic, covers exactly that plan, and binds
  its original execution envelopes and one pool. Store immutable per-job retry
  bounds separately from mutable execution state.
  Exact registration replays use only durable state, even after termination or
  loss of artifact/catalog access. Registration batches native artifact checks
  with one plan load and credential fetch per job; refill trusts registered refs,
  and workers verify the original artifact digest before execution.
- Admission reserves one canonical table and global/organization/pool job slots.
  Keep those reservations through `ReadyToCommit`, `Committing`, cancellation,
  and unknown publication. Only a terminal transition releases them. An unresolved
  publication barrier also prevents another job acquiring that table.
- Serialize short admission and grant budget decisions using one PostgreSQL guard
  row. Managed grants recheck ownership, pool, capability, session capacity, active
  attempt slots, and aggregate memory/CPU reservations inside the grant transaction.
  External I/O never occurs inside these transactions.
- Rebuild a bounded ready cache from durable `Pending` tasks and admitted jobs.
  Cache entries contain identifiers and dispatch fields, never file inventories.
  Rotate a deterministic task-ID window so one pool cannot monopolize a small
  cache. Cache rotation is expendable; task state, ownership, and timers are durable.
- A new scheduler fences dispatch across every clone of its worker service.
  Check the acquired leadership epoch, reconcile incomplete publications using
  retained evidence without resending, repair cancellations and expired attempts,
  then rebuild the queue before opening dispatch. Existing acknowledgment recovery
  remains available. Periodic passes repair missed notifications; correctness
  does not depend on `LISTEN/NOTIFY`.
  Healthy periodic validation does not interrupt previously validated dispatch;
  a failed pass closes it. Dropping or aborting the scheduler runtime also closes
  dispatch. Readiness errors carry `SchedulerNotReady` / `RetrySameRequest`.
- Validate the original canonical table identity, exact planning baseline,
  retained metadata/manifest files, planned source/delete existence and sizes,
  and current catalog authority before admission or retry release. Coordinator
  verification reads metadata only. Drift or lost planning retention requires
  `ReplanRequired`; missing/changed source objects under an unchanged baseline and
  revoked authority fail permanently. Transient catalog/storage failures fence
  dispatch and preserve execution state and retry timers.
  Source metadata probes use ordered concurrency bounded at sixteen, with a
  configurable per-job validation deadline. Queue refill uses two limited indexed
  identifier ranges rather than sorting the whole pending backlog.
- Execution retains the three-attempt budget. Failure or lease/session expiry
  creates one durable retry deadline, using bounded exponential backoff with
  deterministic equal jitter from task identity and generation. Reconciliation
  never moves a future deadline or reuses an old attempt. It releases a due retry
  only after external validation; actual dispatch still checks capacity/authority.
- Terminal transitions and pre-publication cancellation fence all unfinished
  attempts/tasks atomically. Accepted receipts and accepted tasks remain immutable
  for audit and output retention. Fencing does not prove late writers have stopped.
  Live cancelled attempts still receive cooperative cancellation directives;
  fencing does not silently remove their shutdown grace period.

## Consequences

Startup and periodic reconciliation use the same path. A failed pass keeps new
dispatch closed while heartbeats and acknowledgment recovery remain available.
Catalog outages cannot authorize replacement native execution or publication
replay. Restart tests reconstruct the same initial ready window and unchanged
retry timers, including expired worker sessions and exhausted attempt budgets.

This supplies a coordinator library runtime. Worker subprocess supervision is
BS-015; application startup/packaging and user-facing admission configuration
remain their existing backlog work. Admission callers must reconstruct catalog
bindings from durable job identity and register complete immutable artifact refs.
