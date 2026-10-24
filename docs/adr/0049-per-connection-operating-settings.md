# 0049. Per-connection operating settings

- Status: Proposed
- Date: 2026-10-23
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-022.T3

## Context

`docs/operations/catalog-connections.md` asks every connection to define two
bounds: the maximum execution duration (E) of one job, and the maximum
unresolved-publication age (U). Both exist because Bergsmith cannot make an
external cleaner respect its own reservations. The operator states how long the
table's inputs and outputs must stay readable, and Bergsmith keeps its own work
inside that promise.

Until now Bergsmith held neither bound. The job duration budget was a
deployment-wide configuration value, so two connections serving tables with
different writer cadences could not each state their own budget. The
unresolved-publication age was not held at all: the ledger reported the oldest
unresolved publication globally, and the operator had to compare that number
with a bound kept in their head.

Operators also need a way to pause maintenance on one connection without
taking the connection out of service. Disabling a connection (ADR 0047) stops
previews as well, which is the wrong lever for a table that must keep serving
reads while its maintenance is paused.

## Decision

Each connection stores three settings: the maximum execution duration in
seconds, the maximum unresolved-publication age in seconds, and whether
maintenance may run. They are part of the connection's configuration: they
change through the same revision-checked `PATCH /api/v1/catalogs/{id}` as the
name and the routing, and every change is audited with the new bounds.

The execution budget is 1 to 86 400 seconds and defaults to 180 seconds, which
fits a five-minute writer cadence. The unresolved-publication age is 1 to
2 592 000 seconds and defaults to 3 600 seconds. A value outside its range is
refused; omitted fields keep their stored value.

The budget is enforced on the whole job, not on one attempt. The job's plan
carries a deadline, written with the plan from the database clock: the budget
runs from the moment the plan is recorded, which is right after the planning
baseline was captured and is therefore the moment the operator's promise that
the inputs stay readable begins. Every attempt and every retry of that job
must fit before the deadline: a grant and every later heartbeat cap the
attempt's lease at it, the envelope a worker receives carries only the time
that is left (nanoseconds included, so a positive budget below one second
stays a positive duration), and a job whose budget is spent starts no further
work, fences a renewal, and ends with `ExecutionBudgetExceeded`. The deadline
bounds execution only: a publication that was already authorized is never
disturbed by it, and its recovery keeps its own rules. The coordinator's own
task limit stays authoritative, so a connection may only shorten a job.

A job created before this decision carries no deadline and is therefore
unbounded; it keeps the worker's own lease. Storing the deadline with the plan
is what makes the budget survive a restart and cover every attempt of the job.

The unresolved-publication age is the age at which an operator is expected to
investigate. The connection detail reports how many publications of that
connection are unresolved, how old the oldest is, and whether that age is past
the connection's own bound. The bound never resolves a publication and never
stops a job.

Pausing maintenance leaves the connection in service. Previews, reads, and the
recovery of an unresolved publication continue; no new maintenance job is
admitted and no schedule occurrence creates one. A due occurrence records
`MaintenancePaused` and advances its cursor. A job that was already admitted
continues, exactly as it does when a connection is disabled.

The permission and the maintenance eligibility must hold on the same
connection. A requester who may only use a paused connection has no usable
route, even when another connection serves the same table, and the runtime
resolves a connection that allows maintenance for new work.

The effective storage profile stays in the coordinator configuration, so it is
reported read-only. The detail names the backends that serve the connection
and whether the connection has an entry of its own or uses the default set, so
an operator notices a mistyped connection key. A profile names an endpoint and
an addressing, never a credential: those come from the catalog. An endpoint is
reported only when it is an HTTP(S) URL with a host, and without user
information, query, or fragment. Anything else — a string that is not a URL,
or a URL-shaped string whose payload is opaque — is left out, because the
configuration accepts any string and it may carry a secret.

## Consequences

The operator's two bounds now live with the connection instead of in a
document, and Bergsmith's own job duration cannot outrun the budget the
operator kept the table's inputs readable for. A connection can be paused for
maintenance without losing visibility of it.

A deadline is a durable fact of the plan, so a job's budget survives a
coordinator restart and is shared by every attempt of that job. A job that
outruns its budget fails instead of running longer, which is what the
operator's retention promise requires; the coordinator cannot make an
external cleaner keep the inputs longer than it promised.

The bounds are per connection, so a deployment with many connections carries
the same settings many times; a deployment-wide default would be a
configuration concern, not a per-connection one. The stored values are read
through a validated type, so a row outside the supported range is a ledger
error rather than a silently clamped bound.

Moving the storage profiles into the ledger stays open: the decision keeps
them in the configuration file, and only the effective set is reported.
