# 0033. Every job names its requester

- Status: Accepted
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4 BS-017, §5 Security and deployment

Refines [0032](0032-oidc-sessions-and-scoped-authorization.md); that decision stands.

## Context

Scoped authorization checks a job's requester against its table on admission
and before a send. Jobs created before BS-017 have no `created_by`, so the
first implementation returned success when it was `NULL`. That kept in-flight
work running across the upgrade, but it was the only path that authorized an
action without an active grant, and it was invisible to the schema: any forgot
`created_by` — a retry command, a schedule, a backfill — would silently inherit
the bypass.

## Decision

- Every job records a requester. `jobs.created_by` is `NOT NULL` and
  `NewJob.created_by` is a required `PrincipalId`; a writer that omits it fails
  the insert instead of skipping authorization.
- Migration `0012_job_requester.sql` attributes existing unattributed jobs to a
  per-organization `bergsmith:system` service principal, grants it an
  organization-wide `MaintenanceService` role so in-flight work finishes, and
  records the provisioning in the audit log. Revoking that grant is the
  documented way to halt legacy work.
- `require_job_permission` always evaluates a grant. There is no unattributed
  branch.
- Replacement and retry jobs carry the *requesting* principal, never the source
  job's actor, so authority is re-established for each new job.
- The first administrator is provisioned out of band:
  `bergsmith-coordinator bootstrap-admin` creates the principal and its
  organization-wide `Administrator` grant idempotently and audited as a system
  action. Login still never provisions (0032).

## Consequences

- Authorization has no implicit-allow state; every job decision names an actor
  and is revocable.
- An upgrade performs a one-time data migration; organizations with no
  unattributed jobs get no system principal.
- Operators bootstrap deployments with one command and can rotate or revoke
  the system grant exactly like any other.
- A forgotten `created_by` surfaces as a `23502` at insert time, in tests and
  in production, rather than as a silent privilege.
