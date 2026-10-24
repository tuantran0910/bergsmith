# 0054. Session permission projection

- Status: Proposed
- Date: 2026-10-24
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-023.T3

## Context

Authorization is decided from active grants: a `Role` names a set of
permissions and a `GrantScope` names where they apply (ADR 0032). The browser
received the grants from `GET /auth/session`, but it had no permission view, so
navigation and actions were gated on role names. `canReadAudit` and
`canManageWorkers` looked for an organization-wide `Administrator` grant
directly, and `canManageCatalog` repeated the same rule for connections. That
duplicates the role-to-permission table in TypeScript. Giving a role a new
permission, or changing which role carries one, would leave the UI silently
wrong in both directions: showing controls the API denies, or hiding controls
the API would allow.

## Decision

`GET /auth/session` also returns `permissions`, an array of
`{"permission": <stable name>, "scope": <the grant's scope>}` pairs.

- The pairs are expanded on the server from the same `Role`/`Permission`
  mapping that authorization uses, deduplicated, and sorted by permission and
  scope. The browser never interprets a role name to decide what it may do.
  The existing `grants` array stays for grant administration displays.
- The projection is computed from active grants on every read; it is not
  stored in the session row, so a revoked grant disappears on the next session
  read without forcing a new sign-in. Deactivation already ends the sessions
  and refuses admission, so a deactivated principal cannot read a projection.
- The scope is the grant's own scope. An organization pair covers every
  resource; a connection pair covers that connection and, on the server, its
  tables. A table pair covers only its table. The browser helper for a
  connection therefore accepts an organization pair or a pair naming the same
  connection, and a table-scoped pair never authorizes the connection itself.
- The projection is an affordance, never authority. Every API route and
  command re-checks the ledger, and a stale projection only changes what the
  browser offers: a denied request remains the authoritative answer.
- Permission names are the stable strings from `Permission::as_str`, already
  used by authorization errors and audit records, so adding a permission does
  not change the response shape.

## Consequences

- The web helpers `canReadAudit`, `canManageWorkers`, and `canManageCatalog`
  are implemented with `hasOrganizationPermission` and
  `hasConnectionPermission`; no browser code branches on a role name.
  `hasTablePermission` mirrors the server's table check: an organization
  grant, a grant on the table itself, or a grant on any connection the table
  is named on. The table page gates preview and submission with
  `jobs:submit`, and the job page gates cancellation and retry with
  `jobs:cancel` and `jobs:retry`, alongside the existing lifecycle checks.
- A changed role-to-permission mapping reaches authorization and the session
  from one table. Browser fixtures mirror that table so a journey cannot pass
  with an impossible session.
- The response grows by at most one entry per permission and active scope
  (bounded by 11 permissions), computed from grants the handler already reads.
- `/auth/session` remains outside the `/api/v1` OpenAPI contract and is typed
  by hand in `web/src/api/auth.ts`; a route test asserts the expanded,
  deduplicated, and sorted pairs and their refresh after revocation.
- Journey tests cover a Viewer with no maintenance actions and grants that
  cover another table or another connection, plus the positive case of a
  connection grant for an alias.
