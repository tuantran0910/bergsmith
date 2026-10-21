# 0039. Schedule occurrence admission and coalescing

- Status: Accepted
- Date: 2026-10-21
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §5 "Scheduling semantics"; BS-019.T2

## Context

BS-019.T1 added policies, schedules, and the cron/DST calculation, but nothing
ran them. The plan requires the coordinator to:

- persist occurrence identity before admitting a job;
- coalesce missed occurrences into one pending evaluation;
- coalesce overlapping runs for the same table;
- recheck current authorization and the policy at admission and publication;
- stop future admission when a schedule or policy is disabled;
- never replace a conflicted job or reuse its outputs.

The questions this decision answers are how an occurrence is identified, how
downtime and overlap are represented without losing work or creating a burst,
and where a disabled policy takes effect.

## Decision

### Due state

Each enabled schedule has one `maintenance_schedule_state` row with
`next_due_at`. The row is:

- armed on creation to the first occurrence after the database clock, so
  creating a schedule does not run it immediately;
- re-armed from "now" when the timing changes or the schedule is enabled;
- deleted when the schedule is disabled.

Deleting the row is what makes "disabled" mean "not due": no cursor advances
while disabled, and re-enabling starts at the next occurrence instead of
backfilling the disabled interval. Re-sending `enabled: true` for an already
enabled schedule is not a transition and preserves an overdue cursor; only an
actual disabled-to-enabled change (or a timing change) re-arms it.
`next_due_at` is `NULL` when the expression names no reachable instant (for
example `0 0 30 2 *`), which leaves the schedule dormant rather than
busy-looping.

Schedules persisted before this task have no state row. The first scheduler
pass arms each one at its first occurrence after that pass's instant, exactly
as creation does, so an upgrade does not silently strand existing schedules.
Timing that cannot be parsed is also stored dormant (`NULL`) rather than
retried forever, so it cannot monopolize the bounded scan; editing the
schedule repairs and re-arms it.

### Occurrence identity

`maintenance_schedule_occurrences` stores one row per evaluation:

- `scheduled_for` is the **first** due instant the evaluation represents.
- `coalesced_until` is the **last** instant folded into the same evaluation
  when the coordinator was down for several occurrences; it is `NULL` for a
  single occurrence.
- `UNIQUE (organization_id, schedule_id, scheduled_for)` is the idempotency
  key. A replayed evaluation cannot create a second job; the state cursor and
  the unique constraint are two independent guards.
- `state` is `Pending` while the transaction decides, then `Admitted` (with the
  created `job_id`), `Coalesced` (folding into the nonterminal job that already
  owns the table, with a stable reason), or `Skipped` (no job, with a stable
  reason such as `PolicyDisabled` or `ServiceIdentityIneligible`).

Only `Admitted` rows back a job for the publication policy check; a coalesced
row may name the blocking job without making that job "scheduled".

### One atomic admission

`occurrences::admit` runs in one transaction:

1. Fence the coordinator epoch.
2. Lock the schedule row, so a concurrent edit cannot change timing under the
   decision, and lock its state row.
3. Return without writes when no instant is due.
4. Record a `Skipped` occurrence when the policy is disabled or the service
   principal no longer satisfies the T1 eligibility check (a `Service`
   principal with `jobs:submit` on the table). The cursor advances either way.
5. Insert the occurrence as `Pending`.
6. If any job on the table is nonterminal (`Planning` through `Cancelling`),
   mark the occurrence `Coalesced` with reason `TableBusy`. This includes jobs
   created by another schedule or by hand.
7. Otherwise create a `Planning` job with `created_by` set to the schedule's
   service principal. The existing planning, admission, execution, and
   publication paths then treat it like any other job: budgets, table
   exclusivity, retries, and conflict handling are unchanged, and a later
   occurrence creates a fresh job and plan rather than reusing outputs.
8. Advance `next_due_at` to the first occurrence after the current instant and
   commit. An audit record (`job.submitted` for admitted occurrences,
   `job.admission_denied` for skipped ones) commits in the same transaction.

The latest missed instant is found with one backward cron search
(`previous_occurrence`) instead of replaying every missed instant, and the next
cursor is computed directly from the current clock. A long outage therefore
cannot produce a backlog, a burst, or seconds of CPU work inside the fenced
transaction.

Each pass reads a bounded window of due schedules that rotates by schedule
identifier. A schedule whose evaluation keeps failing leaves its cursor
unchanged, but the next pass starts beyond it and eventually reaches every
other due schedule.

### Disabled policy at publication

A scheduled job is blocked from publishing while its policy is disabled:

- the scheduler stops a `Queued`/`Running` scheduled job as `Failed` with
  reason `PolicyDisabled`;
- intent creation and send authorization reject the job with
  `MaintenancePolicyDisabled` before writing an intent or sending a request;
- a denial before send authorization conclusively proves non-publication, so
  the job resolves as `Failed` and its barrier is released. The resolution has
  separate checks for a `ReadyToCommit` job (no intent may exist yet) and a
  `Committing` job (no send may have been authorized), under the job lock with
  the expected revision and epoch. A failed job releases its table, so later
  occurrences do not coalesce into work that can never publish.

Disabling only the **schedule** does not affect a job that already exists:
the state row is removed, so no new occurrence is admitted, while cancelling a
running job remains an explicit operator command.

### Time zones and DST

Admission uses `CronSchedule::next_occurrence` unchanged: spring-forward gaps
produce no occurrence, and a repeated fall-back local time produces one
occurrence at the earlier instant. The integration tests pin both transitions
against a real schedule row and PostgreSQL cursor.

## Consequences

- A scheduled job is created in `Planning` and can exceed the admission budget
  until the existing scheduler admits it. That is the same behavior as a manual
  submission and keeps budget accounting in one place.
- The coordinator `serve` runtime (BS-020.T1) must plan scheduled jobs, as it
  must for manual ones; this task wires occurrence admission into the existing
  scheduler pass, not the planning worker.
- Occurrence rows are an internal ledger. No HTTP resource exposes them yet;
  operators see admitted jobs and their audit records.
- The occurrence row links a scheduled job back to its schedule and policy, so
  the `serve` runtime can plan that job with the policy's target file size and
  minimum input-file count. Applying those parameters belongs to the planner
  wiring in BS-020.T1; manual jobs keep the planner defaults.
- Windows and per-run budgets (BS-019.T3) extend the policy/schedule rows and
  the admission decision; they do not change occurrence identity or coalescing.
