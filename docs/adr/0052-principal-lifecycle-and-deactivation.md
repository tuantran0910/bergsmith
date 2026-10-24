# 0052. Principal lifecycle and deactivation

- Status: Proposed
- Date: 2026-10-24
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-023.T1

## Context

Principals and grants were created only by `bootstrap-admin`,
`bootstrap-service`, and the BS-020.T7 commands. There was no way to list who
belongs to an organization, provision a person through the browser API, or stop
someone's access without revoking every grant they hold, which erases the
record of what they were allowed to do and makes restoring access a
reconstruction rather than a decision.

Access also had to stop for work that does not sign in: a schedule's service
identity and the coordinator's admission and publication checks evaluate
grants directly, so a browser-only change would not stop them.

## Decision

A principal gains `deactivated_at`, and deactivation is a lifecycle change,
never a deletion.

- The row, its grants, and its audit history stay. Reactivation restores the
  same access, because the grants were never removed.
- Authorization requires an active principal. `has_permission` — which every
  command, admission check, publication check, and send authorization uses —
  returns false for a deactivated principal regardless of its grants. The
  inventory read filters make the same check.
- Session lookup requires an active principal, so a session issued before the
  change is rejected even if its revocation was somehow missed. Deactivation
  also revokes every session in the same transaction, and sign-in refuses a
  deactivated principal and records `auth.login_failed` with the reason
  `identity_deactivated`.
- Running jobs are not cancelled and are not rewritten: deactivation stops the
  next admission or publication decision, like grant revocation.
- Deactivating the organization's last active administrator is refused with
  `last_administrator`. Two concurrent deactivations must not each see the
  other as a remaining administrator, so the check and the write run under an
  organization-scoped transaction advisory lock. A repeated deactivation (or
  reactivation) returns the stored row, changes nothing, and writes no audit
  record.
- Principal administration — list, provision, deactivate, reactivate — requires
  `grants:manage` over the organization, the same permission the audited
  operator path already used for provisioning. The issuer and subject are the
  key sign-in is bound to, so reading or changing them is access
  administration, not inventory.
- Sign-in keeps the stored display name in step with the ID token: when the
  provider's chosen name differs, it is written before the session is issued,
  clamped to the 200 characters the ledger accepts. A failed refresh does not
  fail a valid sign-in.

Provisioning through this API creates people only. Service identities and their
pool tokens are BS-023.T4.

## Consequences

- Deactivation is reversible and auditable, and administrators can see who
  belongs to the organization without reading PostgreSQL.
- A deactivated principal still exists for foreign keys and for audit
  attribution; queries that count people must decide whether to include it.
- The last-administrator guard serializes principal lifecycle writes per
  organization. The lock is held only for the transaction, so unrelated
  organizations are unaffected.
- A display name can change at sign-in without an administration command; the
  provider remains the authority for people's names.
