# 0031. Worker renewal and grant boundaries

- Status: Accepted
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2, §5, BS-013

Refines [0023](0023-worker-sessions-and-durable-pulls.md) and
[0024](0024-worker-receipt-fencing-and-task-envelopes.md); other decisions stand.

## Context

Credential refresh follows a committed lease renewal. Returning an RPC error
when refresh fails hides that renewal from the worker. Refreshing on every
heartbeat also adds unnecessary catalog traffic. A grant drafted outside the
transaction can contain a generation that differs from the one allocated under
the ledger lock, and an unusable offer can remain in the ready queue indefinitely.

## Decision

- Return a committed lease renewal even if credential lookup or refresh fails.
  Log the typed reason and omit refreshed credentials for that heartbeat; retry
  refresh on later heartbeats. Existing credentials remain usable until expiry.
- Refresh credentials when their delivered expiry is within one configured
  attempt lease (90 seconds by default), or expiry is unknown. Keep only expiry
  bookkeeping in coordinator memory; never persist credential values. An empty
  credential grant for development storage suppresses refresh for one lease.
  That hint expires on the monotonic clock and is pruned on later deliveries,
  so vanished workers cannot accumulate no-credential entries indefinitely.
- Worker-facing gRPC failures carry a stable typed reason and disposition.
  Credential refresh errors after renewal do not become heartbeat failures.
- Include the drafted generation in the internal grant payload and compare it
  with the generation allocated under the ledger lock. A mismatch rolls back
  the grant so the offer can be drafted again against current state.
- Drop offers whose ledger errors prove them unusable. Preserve offers after
  session, leadership, admission, or database errors so a later poll can retry.

## Consequences

A catalog refresh outage no longer conceals a renewed lease, and known valid
credentials avoid repeated refresh calls. Expired credentials still stop storage
access and follow the execution retry policy. Generation races cannot persist a
grant inconsistent with its attempt row. These changes do not alter protobuf
contracts or permit credentials in the durable execution ledger.
