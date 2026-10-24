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
  joins the principal and returns false for a deactivated one regardless of its
  grants, in the same query as the grant lookup. The inventory read filters
  make the same check.
- Session lookup requires an active principal, so a session issued before the
  change is rejected even if its revocation was somehow missed. Deactivation
  also revokes every session in the same transaction, and sign-in refuses a
  deactivated principal and records `auth.login_failed` with the reason
  `identity_deactivated`. Sign-in reads the principal under a share lock in the
  same short transaction that inserts the session: a deactivation either waits
  for the sign-in and its revocation then sees the new session, or it commits
  first and the sign-in observes the deactivated row. The OIDC exchange happens
  before the transaction, so no network call is held.
- Running jobs are not cancelled and are not rewritten: deactivation stops the
  next admission or publication decision, like grant revocation.
- Deactivating the organization's last active administrator is refused with
  `last_administrator`. Only organization-scoped Administrator grants count: a
  connection- or table-scoped administrator administers that resource but cannot
  manage principals or restore organization administration, so it is not a
  replacement.
- Two concurrent lifecycle changes must not each authorize against the state
  the other is about to replace, so the organization-scoped transaction
  advisory lock is taken *before* the actor is authorized and held through the
  change and its audit record. An administrator whose own access changes while
  a request waits is therefore refused, instead of reactivating themselves
  against the state the other change just committed.
- A repeated deactivation (or reactivation) returns the stored row, changes
  nothing, and writes no audit record.
- Principal administration — list, provision, deactivate, reactivate — requires
  `grants:manage` over the organization, the same permission the audited
  operator path already used for provisioning. The issuer and subject are the
  key sign-in is bound to, so reading or changing them is access
  administration, not inventory.
- Sign-in keeps the stored display name in step with the ID token: when the
  provider's chosen name differs, it is written after the session transaction
  commits, clamped to the 200 characters the ledger accepts. A failed refresh
  does not fail a valid sign-in.

Provisioning through this API creates people only. Service identities and their
pool tokens are BS-023.T4.

## Consequences

- Deactivation is reversible and auditable, and administrators can see who
  belongs to the organization without reading PostgreSQL.
- A deactivated principal still exists for foreign keys and for audit
  attribution; queries that count people must decide whether to include it.
- The last-administrator guard serializes principal lifecycle writes per
  organization, including their authorization. The lock is held only for the
  transaction, so unrelated organizations are unaffected.
- A display name can change at sign-in without an administration command; the
  provider remains the authority for people's names.
