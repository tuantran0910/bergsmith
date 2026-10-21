# 0040. Schedule windows and per-run input budgets

- Status: Accepted
- Date: 2026-10-21
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §1, §4 BS-019.T3, §5 "Scheduling semantics"; ADR 0038, ADR 0039

## Context

BS-019.T1 and T2 made schedules run, but every occurrence had two freedoms the
operator could not bound:

- it could start at any local time the cron expression named, and
- it could rewrite as much input as the global planner and admission limits
  allowed.

The plan asks for windows and budgets in M5, and §5 requires that admission and
publication recheck "current authorization and policy"; a per-run budget is
fixed in the immutable plan. §1 explains the purpose of a budget: a streaming
sink commits about every five minutes, so a run must be sized to fit between two
commits instead of taking as long as the global job limit permits. ADR 0038 and
ADR 0039 deliberately left both decisions to this task and reserved the policy
and schedule rows for them.

Two questions needed answers:

1. What happens to a due occurrence that would start outside its window?
2. Is a budget a limit on how much one run may rewrite, or on how many runs may
   be admitted?

## Decision

### A window belongs to a schedule and gates start times

- A schedule may name a **daily local-time interval**: an inclusive start minute
  and an exclusive end minute after midnight, in the schedule's own IANA zone.
  A start later than the end wraps past midnight, so `22:00`–`02:00` is one
  overnight window. Both bounds are required or both are absent; `start == end`
  is rejected because it is ambiguous between no time and all day. Use an absent
  window for "any time".
- The window is optional. Without one, the schedule keeps its previous behavior.
- Admission tests the **instant at which the job would be created** (`now`),
  converted to the schedule's zone, not the occurrence's nominal time. A run
  never starts outside the window, even if its occurrence was nominally inside
  it. This is the point of a maintenance window: the table is quiet at the
  moment the work begins.
- An occurrence outside the window is recorded as **one** `Skipped` row with the
  stable reason `OutsideWindow`, covering the due span up to the evaluation
  instant, and the cursor jumps to the first cron occurrence at or after the
  next window opening. It is **not** deferred to that opening as a pending run:
  the run itself is dropped, and the jump only avoids one row per cron tick.
  Deferral would either let a run start after the window closed or require a
  second cursor that can collide with the next occurrence. A missed run during
  downtime is not replayed.
- The skip is routine scheduling, not an operator denial. It is logged at debug
  level and is **not** written to the audit log; the occurrence row is still the
  durable record of the evaluation. Only a disabled policy or an ineligible
  service identity, which need an operator, produce a `job.admission_denied`
  audit event. This keeps a cron more frequent than its window from flooding the
  audit table.
- Window checks run after the enabled-policy and service-identity checks and
  before coalescing. A run that may not start should not fold into another job's
  occurrence. Checks are therefore ordered `PolicyDisabled`,
  `ServiceIdentityIneligible`, `OutsideWindow`, then `TableBusy`.
- The window bounds only the start. A run admitted inside the window may finish
  after it closes. Publication does not recheck the window, because doing so
  would abandon already-written outputs once the window ended.
- The comparison uses the zone's wall clock, so a spring-forward gap shortens a
  window that spans it and a fall-back overlap lengthens it. No instant is
  invented or dropped by the window itself; the cron engine still owns
  occurrence identity.
- Finding the next opening treats a repeated fall-back start as two candidate
  openings. The earlier reading is preferred, but if it is already past, the
  later reading still opens the window. This is deliberately independent of
  cron's deduplication to the earlier instant, so a valid run later that morning
  is not dropped.
- Admission samples its clock **after** taking the schedule and state row locks.
  A scheduler pass that started before a window closed but waited on a lock
  therefore decides with the instant it actually has, not the pass start.

### The budget belongs to a policy and caps one run's input

- A policy carries `max_input_bytes`: the most input bytes one run may rewrite.
  It is bounded at 1 MiB–100 GiB (the plan's maximum input per job) and must be
  at least the policy's target file size, so one replacement file always fits.
  The default is 100 GiB, which preserves the behavior of existing policies.
- The budget is applied at **planning**, through
  `PlannerConfig::for_policy`. That function replaces the target file size and
  minimum input-file count with the policy's values, lowers the small-file
  threshold when a smaller target requires it, and caps the job and group input
  limits by `max_input_bytes`. It takes the coordinator's global admission cap
  as an argument, so the effective budget is the smallest of the policy, base
  planner, and admission values, and the job and group limits stay ordered. A
  policy default of 100 GiB against a smaller admission cap is therefore
  planned at the cap instead of failing later as an oversized job.
- The policy's `serve`-time wiring is BS-020.T1, as ADR 0039 already records for
  the policy's target and minimum file count. This task supplies the validated
  field, the durable column, and the tested mapping; scheduled jobs are not
  planned by a runtime until T1 exists.
- Publication does not recheck the budget. The plan is immutable and records the
  configuration it was built with; tightening a policy later must not invalidate
  a plan that already has accepted outputs. Disabling a policy still blocks
  publication, and authorization is still rechecked, as BS-019.T2 implemented.

### API and schema

- `max_input_bytes` appears on the policy resource and on create/update
  requests. `window` appears on the schedule resource and on create/update
  requests. Window minutes are parsed as wider integers and range-checked by
  `MaintenanceWindow`, so every out-of-day value (negative, above 1439, or
  beyond `u16`) returns `invalid_maintenance_window`, not the generic
  `invalid_request`.
- PATCH keeps the existing optimistic-revision contract. For `window`, an absent
  field leaves the value alone, an explicit `null` clears it, and an object
  replaces it. This is the only tri-state field in these resources; the
  persistence `ScheduleUpdate` uses `Option<Option<MaintenanceWindow>>` for the
  same reason.
- Invalid windows return 400 `invalid_maintenance_window`. A run budget below
  the target file size returns 400 `run_budget_below_target_file_size`; other
  out-of-range policy values keep returning `invalid_policy_parameters`. The
  audit detail for `policy.created`/`policy.updated` and
  `schedule.created`/`schedule.updated` carries the stored values.

## Consequences

- Operators can express "nightly, but only between 02:00 and 04:00, and never
  more than 8 GiB per run". Neither setting changes occurrence identity,
  deduplication, or coalescing.
- A window is a guard, not a queue. Downtime that ends after the window closes
  drops that run instead of starting outside the quiet interval. Documentation
  states this so operators size windows around realistic recovery times.
- The per-run budget is inert for manual jobs, which keep the global planner
  configuration, and for scheduled jobs until BS-020.T1 wires
  `PlannerConfig::for_policy`. Tests pin the mapping now so the wiring cannot
  drift.
- Existing rows migrate without behavior change: policies receive the 100 GiB
  default and schedules receive no window.
- A outside-window skip is written directly as `Skipped` instead of
  `Pending` then updated. This refines ADR 0039's step 5 for evaluations that
  are final on arrival; admitted and coalesced rows still insert `Pending` and
  finish after the job decision in the same transaction.
- Changing a window recomputes a **future** `next_due_at` to the first cron
  occurrence after the decision instant, because a skip under the old window may
  have advanced it past runs the new window allows. An overdue cursor is
  preserved and evaluated under the new window, and re-sending the same window
  is not a change and leaves the cursor alone.
- The update samples its clock **after** locking the schedule row. If an
  admission already recorded the next occurrence and advanced the cursor while
  the update waited, the update's decision instant is later than that
  occurrence, so the recomputation cannot rewind onto it. A pre-lock instant
  could do so and leave every later admission retrying a recorded occurrence
  and failing its uniqueness constraint.
- Every cursor recomputation is also bounded by the **latest evaluated
  occurrence**: `max(coalesced_until, scheduled_for)` across the schedule's
  occurrence history. The next cursor is the first cron occurrence after both
  the decision instant and that bound. This makes cursor decisions independent
  of which clock produced the instant. Admission uses the coordinator clock,
  while window edits, timing changes, and re-enables use the database clock; a
  skew that puts the update clock behind an already recorded occurrence cannot
  rewind onto it, because the bound is the persisted history rather than a
  second clock.
- Window eligibility uses the coordinator's wall clock, sampled after the
  schedule and state locks; arming and cursor re-arming use the database clock.
  The latest-evaluated bound makes a difference between those clocks harmless
  for cursor identity. The window remains a start guard and publication does not
  depend on it.
