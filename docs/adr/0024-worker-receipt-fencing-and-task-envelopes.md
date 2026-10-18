# 0024. Atomic receipt replay, task envelopes, and dispatch reservations

- Status: Accepted
- Date: 2026-10-18
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-013.T2–T4
- Supersedes: the failure-classification policy in ADR 0023; its other decisions remain in force

## Context

Worker reports can race with acceptance by another RPC. A status snapshot does
not prove that a particular receipt was accepted. External catalog/storage calls
can be slow, so holding database connections or the global dispatch mutex during
them makes unrelated worker RPCs unavailable. Retry offers must also preserve the
original immutable task's execution envelope after coordinator restarts.

## Decision

- Receipt preflight and final acceptance share the same locked classification.
  Both compare a replay's canonical digest with the durable receipt. No result
  acknowledgment is derived solely from an attempt-status snapshot.
- Native output verification takes an immutable plan, receipt, storage, and table
  metadata, without a database connection. Reporting releases its connection
  before catalog/storage I/O and reacquires one for atomic acceptance. Heartbeats
  likewise release their connection before credential refresh.
- Malformed output locations, invalid metadata, and Parquet decoding failures
  return `InvalidResult`. External/ambiguous I/O errors remain reporting failures;
  they do not authorize another execution attempt.
- Native retryable failures use `TransientNativeExecutionFailed` or
  `TransientMetadataReadFailed`. The coordinator classifies these and
  `StorageOperationTimedOut` as execution retries within the three-attempt budget.
  Other native errors retain their permanent reason codes. Worker-suggested
  dispositions remain untrusted. BS-014 owns retry release and configurable backoff.
- The first admitted offer persists canonical resource-limit bytes in the
  immutable `task_resource_envelopes` table. Reoffers and grant transactions must
  match them. Changing memory, CPU, duration, or output-byte limits requires a
  new plan and job, including after a restart.
- Queue candidates remain counted against capacity while reserved. The queue
  mutex protects only in-memory selection and reservation changes. Database
  reads, plan reconstruction, and grant transactions happen after releasing it.
  Dropping a reservation restores the candidate on failure or cancellation;
  successful assignments and stale candidates consume their reservation.
- Polls for one session wait for its in-flight reservation instead of selecting
  another candidate. PostgreSQL remains authoritative for the attempt and poll
  association. A grant-decision timeout returns an error; clients repeat the same
  poll ID to resolve a possibly committed assignment.
- Concurrent duplicates of a poll ID share an in-memory gate before dispatch.
  A duplicate cannot persist an empty response while the original prepares its
  grant. Gates retain only active callers and hold no database connection.

## Consequences

An accepted replay remains recoverable without catalog/storage access, while a
different receipt cannot obtain `AlreadyAccepted`. A slow catalog cannot retain
the database connection needed by status or cancellation RPCs. One job's row-lock
wait cannot hold the global queue mutex, and cancellation does not lose queue
capacity or candidates. Resource envelopes are durable independently of volatile
queue state. No protocol message fields or dependency additions are required.

Native/TLS/PostgreSQL regressions cover the receipt race, both reporting and
heartbeat connection release, invalid output classification, transient native
failure propagation, retry envelope changes, and unrelated polling during a
blocked grant. Unit tests cover reservation cancellation and classification.
