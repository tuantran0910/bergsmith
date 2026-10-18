# 0022. Durable publication and conservative recovery

- Status: Accepted
- Date: 2026-10-18
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2, BS-012.T1–T4

## Context

A worker finishing a rewrite does not publish its files. The coordinator must
verify the receipt, prepare a conditional Iceberg commit, and survive a crash
between authorizing that commit and recording its response. A delayed authorized
sender may still publish after coordinator leadership changes.

## Decision

The coordinator verifies receipts against the immutable plan and durable grant.
Verification reads object metadata and bounded Parquet footers, never table rows.
It checks output scope, exact finalized inventory, schema and field IDs, row and
byte counts, column counts and bounds, partition identity, split offsets, and
execution totals. NaN counts are worker-produced metrics: footers do not establish
their exact values, so verification checks their range. Closed attempt objects
remain immutable after reporting; subprocess enforcement belongs to BS-015.

Register attempt prefixes before execution. Accept the canonical protobuf receipt
and finalized object inventory in one PostgreSQL transaction, serialized against
cancellation and lease expiry. The last accepted task moves the job to
`ReadyToCommit`. An exact receipt replay returns its stored disposition without
requiring another object read. Zero outputs are an accepted result, not `NoOp`.

Prepare on the original baseline with the capture catalog. Store the baseline,
catalog connection and table name, ordered accepted receipt identities and
digests, expected snapshot identity, proposal hash, and exact serialized request
and request digest. Intent insertion, acquisition of a unique barrier on the
canonical table identity, and transition to `Committing` commit together. The
intent is immutable in PostgreSQL. Revalidate the baseline before authorizing a
persisted intent, including after restart.

Send authorization remains a separate short transaction, serialized with
cancellation and fenced by leadership and revision. The sender accepts only the
stored request bytes, digest, and destination. It sends once outside the
transaction. Only a conclusive result received by that first sender establishes
non-publication. A crash before recording a rejection loses that proof.

Recovery lists durable `Committing` and `CommitOutcomeUnknown` jobs and performs
read-only snapshot reconciliation. Matching retained evidence establishes success,
including when the snapshot is no longer the current head or cancellation is
pending. Absent or mismatched evidence retains the barrier and outputs. Catalog
failure does not release them. A pending non-success disposition never overrides
matching publication evidence; cancellation takes priority if non-publication is
conclusively established. Terminal state and barrier release are atomic.

No catalog is enabled for application-level replay in this implementation.
Intents explicitly record no idempotency key. Adding replay requires catalog
compatibility certification, durable per-send accounting, and a conservative
persisted deadline; advertising an idempotency lifetime alone is insufficient.

## Consequences

The coordinator exposes durable preparation, publication, recovery, and output
inventory operations. Worker RPC wiring and automatic restart scheduling remain
BS-013–BS-015. Inventory classifies active prefixes, accepted outputs, unresolved
publication, published files, and abandoned attempts without authorizing deletion.
The current storage abstraction cannot enumerate multipart uploads; registered
prefixes preserve their scope for future backend-specific inventory.

Uncertain publication can remain unresolved indefinitely. Operators must retain
the affected snapshot history and objects and coordinate external cleanup. There
is no timeout-based failure, blind replay, catalog abort assumption, or automatic
orphan cleanup. See [publication operations](../operations/publication.md).
