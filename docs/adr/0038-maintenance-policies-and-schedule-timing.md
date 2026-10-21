# 0038. Maintenance policies and schedule timing

- Status: Accepted
- Date: 2026-10-21
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §5 "Scheduling semantics"; BS-019.T1

## Context

M5 adds schedules, so maintenance can run without someone submitting each job
by hand. The plan fixes most of the contract already:

- Five-field cron expressions evaluated in IANA time zones.
- A local time that does not exist during spring-forward is skipped.
- A local time that happens twice during fall-back runs once, at the earlier
  occurrence.
- A schedule executes under an explicit maintenance service identity, and
  disabling a schedule or policy stops future admission.
- Configuration updates use optimistic revisions.

What the plan does not spell out is how a policy and a schedule are stored, how
they relate to a table, and how the timing rules are implemented and tested.
Occurrence rows, deduplication, coalescing, windows, and budgets are BS-019.T2
and T3; this decision only covers the configuration model and the timing
calculation.

## Decision

### Policy versus schedule

- A **policy** is the reusable "what": one operation (`Compact` in the MVP), a
  target file size, and a minimum input-file count. It belongs to the
  organization and names no table, time, or identity.
- A **schedule** is the "when, where, and as whom": one policy, one canonical
  table identity, one cron expression with one IANA time zone, and one service
  principal.
- A policy runs at most once per table. Changing the timing updates the existing
  schedule rather than creating a second row, which keeps occurrence identity
  stable. The policy and table of an existing schedule cannot change; a
  different pairing is a new schedule.
- Both rows carry `enabled`, a monotonically increasing `revision`, and
  `created_by`/`created_at`/`updated_at`. Disabling is an ordinary update, so
  the row and its audit trail survive.

### Timing

- The expression is the classic five-field form: minute, hour, day-of-month,
  month, day-of-week. Each field accepts `*`, numbers, three-letter month and
  weekday names, ranges, steps, and comma-separated lists. Day-of-week accepts
  `0`–`7`, where `0` and `7` both mean Sunday.
- When both day-of-month and day-of-week are restricted, a date matches when
  either matches (the Vixie cron rule). A field is unrestricted when it covers
  its whole range, so `*/1` behaves like `*`.
- The time zone is a canonical IANA name from the bundled time-zone database.
  A name must be `UTC` or contain a `/`, as every regional IANA name does.
  Abbreviations such as `EST`, `MST`, or `HST` are rejected because they name a
  fixed offset and do not describe daylight-saving transitions.
- Resolution walks candidate local times in ascending order and converts each
  to UTC. A local time inside a spring-forward gap yields no instant and is
  skipped. A local time inside a fall-back overlap yields two instants; only
  the earlier one is an occurrence, so an instant between the two readings does
  not produce a second run.
- `next_occurrence` searches at most nine years ahead, which covers the longest
  leap-day gap (2100 is not a leap year). An expression that names
  no date, such as `0 0 30 2 *`, returns `None` instead of looping.
- The calculation lives in `bergsmith-common` and is pure: no database, no
  clock reads. Callers pass the instant to start after.

### Service identity

Creating a schedule requires the named principal to be a `Service` principal
that currently holds a grant carrying `jobs:submit` for the table. A person
cannot be an execution identity, even with an administrator grant, and a grant
for a different table or connection does not qualify. Rechecking the grant at
admission and publication belongs to BS-019.T2, which follows the plan's rule
that current authorization is rechecked at both points.

### API and audit

- `GET`/`POST /api/v1/policies` and `GET`/`PATCH /api/v1/policies/{id}`,
  likewise for `/api/v1/schedules`, replace the previous 501 placeholders.
- Every operation requires organization-wide `schedules:manage`. Schedules and
  policies belong to the organization; a grant scoped to one connection or
  table does not administer them, so listing, reading, creating, and editing
  stay consistent.
- Updates name the revision they read. A stale revision returns 409
  `revision_conflict` and changes nothing.
- Invalid cron syntax, an unknown time zone, out-of-range policy parameters, and
  an ineligible service identity are 400 responses with typed reasons, so the
  UI can point at the offending field.
- Each mutation and its audit record (`policy.created`, `policy.updated`,
  `schedule.created`, `schedule.updated`) commit in one transaction. An audit
  failure leaves the configuration unchanged.

## Consequences

- Occurrence persistence (BS-019.T2) can rely on a validated, immutable pairing
  of policy, table, and timing, and on a schedule identity that does not move.
- Windows and per-run budgets (BS-019.T3) extend the policy or schedule row;
  they do not change the timing contract.
- The cron engine is implemented locally rather than through a dependency. It
  is a small, fully tested surface, and the alternative would add a crate whose
  DST behavior and field semantics would still need certification against the
  plan's rules. The IANA database itself comes from `chrono-tz`, pinned in the
  workspace, because maintaining a time-zone database is not a Bergsmith
  concern.
- A web UI for schedules and policies is not part of this task: the API and the
  generated client are ready, and BS-020.T1 supplies the served runtime that M4
  acceptance needs.
