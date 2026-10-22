# 0043. Worker journal retention

- Status: Accepted
- Date: 2026-10-22
- Plan reference: BS-020.T3; ADR 0026 (durable worker supervision), ADR 0030 (report quarantine)

## Context

Each worker keeps one private directory per attempt in its state directory:
the immutable `result.pb` or `failure.pb`, then `ack.pb` once the coordinator
has decided, or `quarantine.pb` when it never will. Nothing removed settled
directories, so disk use grew without bound, and the one-time startup scan
that rebuilds the pending index read every directory ever created.

The attempt directory also guards against a second execution: a worker
refuses to run an attempt whose directory already exists.

## Decision

- A worker removes an attempt directory once its `ack.pb` is older than
  `journal_retention_hours` (default 720, 30 days; at least one hour). Age is
  measured from the acknowledgment, not from the report.
- Directories without `ack.pb`, and quarantined directories, are never
  removed by retention. Quarantined records are kept for inspection and
  removed by an operator.
- Removal renames the directory into `.trash/` inside the state directory,
  syncs the state directory, and only then deletes it. Every pass first
  empties `.trash/`, and recovery ignores hidden directories, so a crash
  mid-removal never exposes a partial record.
- Removal runs at startup, before the recovery scan, and hourly afterwards. A
  failed periodic pass is logged and retried; a failed startup pass stops the
  worker like any other journal I/O error.

Removing an acknowledged record cannot enable a second execution. A worker
writes `ack.pb` only after the coordinator's definitive decision on an ended
attempt (accepted, stale, or cancelled), and the coordinator never grants an
ended attempt again: a replayed poll for it returns the attempt's disposition,
not its grant.

## Consequences

- Disk use and startup scan cost are bounded by the attempts acknowledged
  within the retention window.
- History older than the window is gone; operators who need it copy
  acknowledged directories out before they expire. No archive format is
  defined.
- An operator may still remove acknowledged directories by hand while the
  worker is stopped, for example when a disk fills.
