# 0005. Commit through Bergsmith's own CommitTableRequest client

- Status: Superseded by [0015](0015-commit-adapter-capture.md)
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Pinned integration baseline; §3 Standard catalog commit adapter

## Context

At the pinned revision, `Transaction::commit` reloads the table on every attempt, re-applies actions on fresh metadata, and retries conflicts. `TableCommit`'s builder is crate-private. iceberg-rust sends no `Idempotency-Key`.

## Decision

- The coordinator runs `ReplaceFilesAction::commit` against the planning-baseline table to obtain updates and requirements, adds the full requirement set, and sends the public `CommitTableRequest` through its own HTTP client.
- That client sends an optional `Idempotency-Key`, performs no implicit retries, and classifies 409, 5xx, timeout, and transport errors itself.
- Bergsmith never calls `Transaction::commit`.

## Consequences

- No iceberg-rust patch is needed for publication.
- Bergsmith maintains its own catalog authentication for the commit call.
