# 0010. Commit-aligned job start and per-job duration budget

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §1 Existing-catalog integration and conflict policy

## Context

Under conservative conflict rejection, a job succeeds only if it fits between two sink commits. With a random start, success probability is roughly `1 - duration / interval`.

## Decision

- A due manual or scheduled job waits, for a bounded time, until Inventory observes a new `main` snapshot, then captures its baseline.
- Admission caps job input so expected duration stays well inside the observed commit interval (initially about 3 minutes at a 5-minute cadence). Large backlogs are compacted over several jobs.

## Consequences

- Most jobs fit inside one commit gap without changing the conflict policy.
- M1 measures the real conflict rate; that data decides when to revisit compatible rebasing.
