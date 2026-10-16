# 0011. Execution ledger schema and access in PostgreSQL

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2 Durable state and in-memory scheduling, Job lifecycle, Cancellation contract

## Context

The ledger must hold its invariants even when application code is wrong:
one plan and one publication per job, one active attempt and one accepted
receipt per task, immutable plans, receipts, and events, and a cancellation
boundary that never moves. It must be testable against real PostgreSQL with
one command, and CI must not need a live database to compile.

## Decision

- SQLx 0.9 with runtime-checked queries (`query`, `query_as`), not the
  compile-time `query!` macros, so building needs no database. Correctness
  comes from integration tests against real PostgreSQL.
- Invariants live in the schema: unique and partial unique indexes,
  composite `(organization_id, id)` foreign keys for tenant isolation, CHECK
  constraints for state names and hashes, and triggers that reject updates to
  immutable rows and to recorded cancellation and send-authorization fields.
- Repository functions take `&mut PgConnection` so callers compose them in one
  transaction. Functions that must be atomic open their own transaction or
  savepoint. Locks are taken job row first, then task, then attempt.
- Job transitions compare the caller's `state_revision`, validate with
  `bergsmith-common`, and write the job event in the same transaction.
- Send authorization sets `publication_started_at` without changing the job
  revision. It checks the coordinator's leadership epoch, the revision, the
  `Committing` state, and the absence of a cancellation request.
- Leadership is one row with a lease by database time and an epoch that grows
  with each new holder. Each coordinator process uses a fresh holder ID.
- Migrations are embedded in `bergsmith-persistence` and applied only by
  `bergsmith-coordinator migrate`.
- Tests use `#[sqlx::test]`, which creates a fresh database per test on the
  server named by `DATABASE_URL`. `make test` starts PostgreSQL from
  `deploy/docker/compose.yaml` when `DATABASE_URL` is unset.

## Consequences

- Schema and query mismatches surface in tests, not at build time, so every
  query needs an integration test.
- Test fixtures may set states directly with SQL to reach a precondition;
  production code changes state only through the repository functions.
- Follow-up: BS-013 adds worker sessions (the attempt foreign key) and the
  leadership check on grants. BS-012 extends publications with commit intents.
  TLS for PostgreSQL connections is enabled when packaging (BS-020).
- Note (BS-005): `make test` now always starts the Compose services
  (`make services-up`); `DATABASE_URL` only selects the server.
