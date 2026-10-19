# 0030. Worker report quarantine and supervision recovery

- Status: Accepted
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2, BS-015
- Supersedes: the reporting refusal, lease-clock, and development resource-limit
  behavior in [0026](0026-durable-worker-supervision.md); its other decisions stand

## Context

A permanently refused report stopped the worker and remained pending after
restart, so it could prevent every subsequent assignment. Worker-local setup
failures also appeared to be permanently invalid grants. Wall-clock skew could
end a valid lease early, and development CPU-time limits could expire early when
several threads ran concurrently.

## Decision

- After a definitive report rejection, query `GetAttemptStatus`. A conclusive
  ended disposition acknowledges the original journal record. Otherwise, after
  three consecutive definitive refusals in the reporting loop, durably write
  `quarantine.pb` with `ReportRejected`. Corrupt report contents use
  `InvalidJournalRecord`. Preserve the original record for inspection and let
  polling resume. Quarantine is a local recovery marker, not coordinator
  acceptance or permission to repeat the attempt.
- Authentication, transient RPC, and local persistence failures retain pending
  reports for retry. Conflicting result and failure records still fail startup.
- Journal failures from beginning an assignment when possible, then continue
  polling. Classify local setup faults as `WorkerLocalFailure`, unverified task
  artifacts as `TaskArtifactUnverified`, and unavailable process memory samples
  as `ProcessMemoryUsageUnavailable`, all subject to the execution retry budget.
  Invalid grant contents remain permanent `InvalidTaskGrant`. Require three
  consecutive failed memory samples before stopping a running task.
- Convert lease expiry to a monotonic deadline using the coordinator HTTP
  `Date` header, subtracting one second for its resolution. Fall back to worker
  wall time when the header is unavailable. Credential expiry continues to use
  worker time, also used for signing storage requests.
- Remove task cgroups after partial setup, launch failure, and process death.
  Dropped supervision kills its cgroup before retrying removal. Startup removes
  only empty leftover Bergsmith cgroups; populated groups remain for inspection.
- Drain subprocess stdout and stderr with bounded lines and forward them with
  the attempt identity. The subprocess never receives service tokens or catalog
  credentials, and storage credentials must not enter logs.
- On development platforms without Linux CPU throttling, size the CPU-time
  backstop for at least every available core over the task duration. Keep Linux
  cgroup CPU bandwidth enforcement and cancellation escalation unchanged.

## Consequences

One refused or corrupt report no longer prevents later work. Operators must
inspect quarantined records; quarantine does not settle the durable coordinator
attempt or prove publication. Lease-clock protection depends on an available
coordinator time header, and storage signing still requires synchronized clocks.
Linux cgroup enforcement still requires delegated-host acceptance before
production certification. Journal archival remains BS-020.T3.
