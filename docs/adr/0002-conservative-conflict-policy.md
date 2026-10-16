# 0002. Conservative conflict rejection; compatible rebasing deferred

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §1 Existing-catalog integration and conflict policy

## Context

Standard Iceberg REST commits can assert only some table state. Target tables are streaming sinks that commit about every 5 minutes.

## Decision

- Every commit sends the original planning-baseline requirements: table UUID, `main` snapshot, current schema ID, last assigned field ID, default spec ID, last assigned partition ID, default sort-order ID.
- Any observed baseline change or failed requirement ends the job as `ReplanRequired`. Outputs are never reused across conflicts.
- Compatible rebasing is deferred beyond the MVP and revisited with the M1 conflict-rate measurement (see ADR 0010 for the cadence mitigation).

## Consequences

- Correctness is simple to reason about and test.
- Jobs on busy tables can fail repeatedly; ADR 0010 reduces this.
- Metadata-only races not covered by standard requirements are documented as outside the maintenance window.
