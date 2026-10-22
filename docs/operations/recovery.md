# Upgrades and recovery

These runbooks rely on one property: every decision Bergsmith acts on is
durable in PostgreSQL before it is acted on, and recovery only reads evidence.
Never edit ledger rows, clear a table barrier, or delete worker journal
records that lack `ack.pb`.

## Upgrades

1. Stop every coordinator. Workers can keep running; they retry their polls
   and reports until a coordinator returns, and keep unacknowledged reports in
   their journals.
2. Run `bergsmith-coordinator migrate` once, from the new release. It is
   idempotent and serializes concurrent runs, but replicas never migrate at
   startup.
3. Start the new coordinator. The first scheduler pass recovers every
   incomplete publication before planning or dispatching new work.
4. Upgrade workers one at a time. Stopping a worker sends cooperative
   cancellation to its task, which is retried under the job's retry budget.

The worker protocol revision must match; a worker refuses an incompatible
coordinator and stops.

## Coordinator restart or crash

Restart it; no manual step is needed. A replacement waits until it can
acquire the leadership lease. After a clean shutdown the lease is released
at once; after a crash it expires after `leadership.lease_seconds` (300 by
default). On startup the coordinator:

- recovers publications: an intent without send authorization is published or
  cancelled, and an authorized send is only reconciled from catalog evidence,
  never re-sent;
- fences attempts whose leases expired and reschedules their tasks within the
  three-attempt budget;
- restores retry timers and pending cancellations from the ledger; and
- re-evaluates due schedules, coalescing occurrences missed during downtime
  into one evaluation per schedule.

Two coordinators started together are safe: only the lease holder serves, and
every planning, dispatch, and publication write is fenced by its epoch.

## PostgreSQL outage

While the database is unavailable, the coordinator closes dispatch and logs
failed passes; workers keep executing granted tasks and retry their reports.
Renewal keeps retrying. If the database returns before the lease expires,
the coordinator continues; otherwise its first renewal fails with
`coordinator leadership lost` and it stops. Run it under a supervisor (such
as Compose or Kubernetes) that restarts it; the restarted process acquires a
new epoch. No step is skipped and
no outcome is guessed: work resumes from the ledger.

## PostgreSQL restore from backup

A restore can forget decisions taken after the backup, including commit
intents that may already have published.

1. Keep every coordinator stopped. Disable catalog writes from Bergsmith by
   not starting `serve`.
2. Restore the database and run `migrate`.
3. For each table maintained since the backup was taken, compare the
   catalog's snapshot history with the restored ledger. A snapshot whose
   summary carries a `bergsmith.publication-id` (and `bergsmith.job-id`) that
   the ledger does not know was published from a forgotten intent; its
   outputs are live table data.
4. Jobs that the restored ledger shows as `Committing` or
   `CommitOutcomeUnknown` are reconciled from that same evidence when the
   coordinator starts.
5. Start one coordinator.

Do not run an orphan cleaner over `<data-location>/bergsmith/` until step 3 is
complete: files from a forgotten publication are referenced by the table even
though the ledger no longer lists them.

## Unknown publication outcomes

A job in `CommitOutcomeUnknown` keeps its table barrier, its admission slots,
and its outputs. Each scheduler pass re-reads retained snapshot evidence and
records `Succeeded` as soon as it matches, even if the table head has moved
on. The coordinator logs the number of unresolved publications every pass.

1. Check that the catalog is reachable and that snapshot history covering the
   send is still retained ([catalog-connections.md](catalog-connections.md)).
   Most unknown outcomes resolve on their own once the catalog answers.
2. If the history is gone, the outcome can only be settled by operator
   attestation; follow [Resolving an unknown outcome](publication.md#resolving-an-unknown-outcome).
   This release has no command for it yet, so leave the job unresolved. The
   table stays blocked, which is safe.

## Worker loss

A worker that dies or loses its network stops renewing its attempt's lease.
The coordinator fences the attempt when the lease expires and retries the task
on another worker, within the three-attempt budget. Restart the worker with
the same `state_directory`: it reports unacknowledged results first, with
their original session, and never runs an attempt it already has a record for.
A different worker name or a fresh directory is also safe; the old session's
attempts expire.

If the state directory is lost, its unreported results are lost too; the
coordinator retries those tasks after their leases expire. Never copy a state
directory to a second running worker.

## Quarantined worker reports

A record the coordinator rejects three times without a conclusive status, or
one that is corrupt, is quarantined: `quarantine.pb` records the reason
(`ReportRejected` or `InvalidJournalRecord`), and the worker keeps polling.
The coordinator has already ended or will expire that attempt, so no action is
needed for correctness. Inspect the record and the coordinator logs for the
attempt ID, then delete the attempt directory while the worker is stopped.
Retention never removes quarantined records.

## Disk full on a worker

Journal writes fail as retryable `WorkerLocalFailure`, and reports stay
pending. Free space by lowering `journal_retention_hours` and restarting the
worker, which removes expired acknowledged records before recovery, or by
removing acknowledged attempt directories by hand while the worker is
stopped. Never remove a directory without `ack.pb`.
