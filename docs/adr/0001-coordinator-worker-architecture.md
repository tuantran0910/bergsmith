# 0001. Coordinator–worker architecture with a PostgreSQL ledger

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §1 Selected architecture; §2 Durable state and in-memory scheduling

## Context

Bergsmith runs distributed compaction for several catalog connections. It needs durable task state, crash recovery, and exactly one accepted result per task, without operating extra infrastructure.

## Decision

- One active coordinator, fenced by a PostgreSQL leadership lease and epoch, plans jobs and owns scheduling, leases, result acceptance, and publication.
- Workers pull tasks over gRPC (`bergsmith.cluster.v1`). The coordinator picks tasks from a bounded in-memory ready queue and persists each grant before returning it.
- PostgreSQL is the durable execution ledger. There is no message broker; workers never poll the database.

## Consequences

- Correctness never depends on cache contents or LISTEN/NOTIFY; reconciliation rebuilds the queue from durable state.
- A standby coordinator is deferred to M8.
- A broker is added only if measured limits require it.
