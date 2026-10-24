# 0053. Scoped grant administration

- Status: Proposed
- Date: 2026-10-24
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-023.T2

## Context

Grants were created only by `bootstrap-admin`, `bootstrap-service`, and the
BS-020.T7 commands, and BS-023.T1 added principal administration. There was
still no browser API to grant or revoke access, so an operator had to hold a
database or CLI session for every access change. The last-administrator rule
already protected deactivation; revocation had no equivalent guard, so
revoking the wrong organization-wide grant could leave nobody able to
administer the organization.

Two limits shape the design. A connection or table administrator already holds
`grants:manage` over that resource, so an API must let them delegate within it
without granting organization-wide access. And revocation is permanent history:
the row records who revoked it and when, so audit can explain access that
existed.

## Decision

`GET /api/v1/grants`, `POST /api/v1/grants`, and
`DELETE /api/v1/grants/{id}` administer active grants.

- A grant names exactly one scope: the organization (both fields absent), one
  catalog connection, or one canonical table. The request carries the scope as
  a nested object; missing fields default to the organization and both scope
  fields together are rejected. The response uses the same shape `/auth/session`
  already serializes, so a later UI can compare them.
- Every route requires `grants:manage` over the scope it reads or changes.
  Create and revoke check it twice: once in the route, so a denial is audited
  with a connection the handler already holds, and once inside the transaction
  that writes the change and its audit record, so a revocation that lands in
  between wins. A connection administrator can therefore grant and revoke on
  that connection and its tables but cannot grant organization-wide access or
  reach another connection.
- The list is keyset-paginated by grant identifier and shows active grants
  only. Without a scope filter it covers the organization and requires
  organization-wide permission. A `catalog_connection` filter requires
  connection permission and also returns table grants for tables aliased on
  that connection, because those grants apply to it; a `table` filter requires
  table permission. The cursor binds the organization, requesting principal,
  and filters, so a page cannot silently change selection midway.
- Revocation sets `revoked_at` and `revoked_by` and keeps the row. It takes
  effect at the next admission, publication, or command because every check
  reads active grants, and it never touches a running job.
- Revoking the organization's last active organization-wide Administrator grant
  is refused with `409 last_administrator`. Only an organization-scoped grant
  counts as a replacement: a scoped administrator cannot manage principals or
  restore organization administration. The check takes the same
  organization-scoped transaction advisory lock as deactivation and
  reactivation, and takes it before authorization, so two concurrent changes
  cannot each see the other's grant as its replacement. The change, the guard,
  and the audit record commit together.
- An identical active grant is refused with `409 grant_already_exists` rather
  than silently returning the existing row: the caller asked for a new
  delegation and the answer should say that it already exists.
- Creating a grant checks only the actor's scope. `grants:manage` is carried by
  Administrator alone, so the role a caller may grant cannot exceed the role
  that authorized it; a connection administrator granting Administrator on that
  connection is delegating the same scope, not escalating beyond it.
- Grant changes are audited as `grant.created` and `grant.revoked` with the
  grant, principal, and role in the same transaction. Route denials and
  transactional races are audited as `api.permission_denied`.

## Consequences

- Access can be delegated without a database session, and a scoped
  administrator sees exactly the grants they can manage.
- Listing a connection's grants also lists its tables' grants, which is more
  than the connection-wide rows but exactly the grants that affect it.
- The last-administrator invariant now holds for both deactivation and
  revocation, serialized per organization. Organizations are unaffected by
  each other and the lock is held only for the transaction.
- Grants held by deactivated principals remain listed because they are still
  active rows; they authorize nothing until the principal is reactivated.
- The API creates grants for existing principals. Provisioning service
  identities and pool tokens remains BS-023.T4.
