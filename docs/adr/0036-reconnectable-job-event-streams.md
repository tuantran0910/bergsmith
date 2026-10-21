# 0036. Reconnectable job event streams

- Status: Accepted
- Date: 2026-10-21
- Plan reference: BS-018.T3, public API and UI

## Context

The public API requires job events over reconnectable server-sent events (SSE)
backed by durable event IDs. Every lifecycle change already appends an
immutable row to `job_events`, whose primary key is a global `bigint`
identity. Two problems prevent exposing that identity as the SSE event ID:

- Identity values are assigned at insert, not at commit. Across jobs, a
  transaction holding a lower value can commit after one holding a higher
  value, so a client resuming "after N" could miss an event.
- The identity is shared by every organization. Gaps between a tenant's IDs
  would disclose other tenants' event volume.

Within one job the order is already safe: `create_job` inserts the job and its
first event in one transaction, and every later append (transitions,
cancellation requests, and send authorization) runs after `lock_job` takes the
job's row lock. Appends to one job are therefore serialized and commit in
append order.

## Decision

### Per-job sequence as the event ID

- Migration 0013 adds `job_events.sequence`: 1 for a job's first event, then
  consecutive. It is assigned as the job's current maximum plus one inside the
  locked append, `NOT NULL`, positive, and unique per `(job_id, sequence)`.
- Existing rows are numbered by identity order per job. The immutability
  trigger is disabled only for that backfill, inside the migration's
  transaction.
- The unique constraint fails closed: an append path that skipped the job lock
  would compute a duplicate sequence and be rejected rather than reorder a
  stream.
- Streams are per job. An organization-wide stream is out of scope, because no
  commit-ordered sequence spans jobs.

### Endpoint

- `GET /api/v1/jobs/{id}/events` returns `text/event-stream`. Each event has
  `id` = sequence, `event` = the kind in snake case, and JSON `data` with the
  sequence, kind, from/to state, state revision, typed transition reason,
  actor principal ID, and time.
- A client resumes with `Last-Event-ID` (sent automatically by a reconnecting
  `EventSource`) or the `after` query parameter; the header wins. Each must be
  a non-negative decimal; anything else, or an unknown query parameter, is 400.
- Authorization is the `jobs:read` predicate of `GET /api/v1/jobs/{id}`,
  checked before any stream bytes. Unknown and inaccessible jobs are both 404.
- Only typed transition reasons are sent. A cancellation request's reason is
  operator text that the job resource does not expose, so it is omitted.

### Delivery

- Streams read from PostgreSQL in pages (500 by default) and hold a pooled
  connection only for each query, never while waiting.
- An `AFTER INSERT` trigger sends `pg_notify('bergsmith_job_events', job_id)`;
  notifications are delivered on commit. One `PgListener` per process forwards
  wake-ups to streams through a bounded broadcast channel. A lagging stream,
  a listener (re)connection, or a malformed payload wakes every stream.
- Correctness never depends on a notification: each stream also re-reads the
  ledger every 2 seconds. A lost notification or listener outage delays
  delivery; it never drops or reorders an event.

### Lifetime and limits

- A keep-alive comment every 15 seconds.
- The session and job access are checked again at most every 30 seconds, and
  a due check runs before every event, including events already read into the
  stream's buffer. The deadline is checked the same way, so a slow consumer
  receives nothing after its access or the stream's lifetime ends.
- Producing each event, including connection acquisition, queries, and
  rechecks, races the stream's deadline. A stalled database call ends the
  stream at its deadline and releases its admission permits. The
  session check does not refresh the idle timestamp: an open stream is not user
  activity. The stream ends, without an error event, on revocation, expiry,
  idleness, or lost access.
- The stream ends after delivering the event that reaches a terminal state,
  since terminal jobs never change, and after at most one hour. A client
  reconnects with the last ID it saw.
- At most 256 concurrent streams per process and 8 per principal. The
  per-principal count is removed when it reaches zero, so admission state is
  bounded by open streams rather than by every principal that ever connected. Excess
  requests receive 503 `event_stream_capacity_exhausted` with
  `RetrySameRequest`. Capacity is released when the client disconnects.

## Consequences

- A client that resumes from its last ID receives each later event exactly
  once, regardless of reconnect timing or concurrent writers.
- Every future `job_events` writer must hold the job row lock (or insert the
  job). The unique constraint turns a violation into a failed write.
- Each coordinator process uses one extra PostgreSQL connection for the
  listener. It is opened from the request pool's connect options but held in
  its own one-connection pool: SQLx's `PgListener` keeps its connection for as
  long as it listens, so sharing the request pool would permanently shrink it.
- HTTP startup is still not wired. A runtime that serves the router must start
  `JobEventHub::listen`; without it, streams fall back to polling.
- The web UI (T4) consumes this stream through the generated client.
