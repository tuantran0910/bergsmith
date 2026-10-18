# 0023. Worker sessions and durable pull assignments

- Status: Accepted; failure classification superseded by [0024](0024-worker-receipt-fencing-and-task-envelopes.md)
- Date: 2026-10-18
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-013.T1–T4

## Context

A lost grant response must not start another execution. Worker restarts, expired
leases, and cancellation must prevent new results from gaining publication
authority while preserving recovery of acknowledgments already committed.

## Decision

- Every cluster RPC requires server-authenticated TLS and a pool-scoped bearer
  token. Configured verifiers bind tokens to an organization, service principal,
  and pool. Sessions and attempts bind all subsequent calls to that identity.
- Registration creates a fresh session. Reusing a worker name under the same
  identity supersedes its previous session. Supersession fences renewals and new
  result acceptance; it does not prove the old process has stopped writing.
- Protocol revision 2 adds an ended-assignment disposition to poll responses.
  Earlier revisions must upgrade before registration.
- A poll UUID is scoped to its session. Its immutable record contains either
  no assignment or one attempt and the safe grant fields. A grant transaction
  creates the attempt, records its output prefix and poll association, and moves
  the first queued job to `Running`. The transaction commits before any reply.
- At most one active attempt is assigned to a session. A bounded in-memory queue
  accepts explicit task offers; selection checks tenant, pool, capability, memory,
  and CPU. Automatic admission and reconstruction remain BS-014.
- Database wall time is sampled after serialization locks. Grant and renewal
  transactions check current coordinator leadership. An expired session or
  generation cannot be renewed or contribute a newly accepted result.
- Poll replay recovers the original assignment or its current disposition.
  Storage credentials are freshly loaded outside transactions, never stored in
  PostgreSQL or the task artifact. Heartbeats return refreshed credentials.
- Result reports verify the canonical digest, session ownership, and generation,
  then use BS-012's native verification and atomic acceptance. An identical
  accepted receipt returns `AlreadyAccepted` even after expiry or cancellation,
  without accessing the catalog or output objects. A conflicting replay is invalid.
- Failure reports persist the stable reason and authoritative classification;
  worker messages and suggested dispositions are not persisted or trusted.
  Storage-operation timeouts may enter `RetryWaiting` within the three-attempt
  limit, initially with a one-second delay. Configurable backoff, revalidation,
  and releasing retries remain BS-014. Other failures conservatively fail or
  require replanning. Duplicate reports do not consume another attempt.

## Consequences

Poll and receipt acknowledgment recovery works with an empty coordinator queue.
Draining stops new assignments while allowing existing attempts to finish.
Legacy attempts receive expired migration-only sessions rather than new execution
authority. Objects under old attempt prefixes stay inventoried for later cleanup.

The service is a library component with a catalog-backed access provider and a
TLS configuration helper. Runtime scheduling, startup reconstruction, worker
subprocesses, and production packaging remain separate backlog work. The TLS
helper selects an initial rustls provider because upstream HTTP dependencies
enable both ring and AWS-LC; an operator-installed provider is preserved.

Acceptance tests exercise real TLS/tonic, PostgreSQL, and native Iceberg/Parquet
files. Separate lock-wait tests drive expiry through database state rather than
sleep-based synchronization.
