# 0051. Catalog connection pages and draft checks

- Status: Proposed
- Date: 2026-10-24
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-022.T5

## Context

Connection management, capability diagnostics, settings, and registration are
available through the APIs in ADRs 0047–0050. Operators need those workflows in
the browser. The setup flow must check capabilities before saving, but the
saved-connection diagnostic needs an existing connection ID.

## Decision

Add `/catalogs`, `/catalogs/new`, and `/catalogs/{id}` to the typed web routes.
Inventory readers see authorized summaries and registered tables. Administrator
grants covering a connection expose settings, diagnostics, discovery, and
registration controls; creating a connection needs an organization grant. The
server remains the authorization authority.

`POST /api/v1/catalogs/check` accepts the create-connection fields and checks an
unsaved draft. It requires an authenticated session, CSRF, and organization-wide
`connections:manage`. Validation and the probe match the saved check, sharing its
10-second timeout, response byte cap, and process/requester concurrency limits.
No database connection or transaction spans network I/O. Permission is checked
again before recording the successful diagnostic. The answer contains a database
timestamp and capabilities. The audit uses `catalog.checked` with no resource ID
and contains only the timestamp, completeness, and missing requirement names.
No connection or saved capability row is created, even on success.

The browser checks a draft before offering Save. Editing any field discards
the diagnostic. Missing required endpoints require explicit acknowledgement;
unknown vended credentials remain diagnostic, not certification. A failed probe
does not offer Save. Credential reference resolution remains deferred, so this
guided setup currently serves catalogs accepting unauthenticated requests. The
draft result is not attached to the saved connection; a subsequent saved check
records its current routing fingerprint under ADR 0048.

Settings writes carry the revision read and only changed fields. A conflict or
uncertain update requires an explicit reload before editing again. A create
whose outcome is uncertain is not replayed: the operator must inspect the list
before creating another connection. There are no automatic mutation retries.

Discovery follows cursors and preserves namespace levels as arrays, including
literal dots. Operators can register one table, an entire namespace under the
server's bounds, or a known name when namespace listing is unavailable.
Registered names come from the ledger, so names dropped from the catalog remain
removable. Removal requires confirmation and sends the observed alias revision;
it never deletes the Iceberg table or job history. Storage profiles remain
read-only and contain no credential values.

## Consequences

Operators can connect and register tables without command-line access. The UI
uses the generated API client, existing design system, session fencing, and
same-origin transport. Browser journeys exercise creation, discovery,
registration, settings, secret-bearing URI rejection, and removal under the
coordinator's CSP in both themes, with accessibility checks.

The unsaved probe adds an administrator-only outbound request entry point, with
the same network authority and resource limits as creating and checking a saved
connection. It does not make catalog authentication or credential reference
resolution available. Discovery and registration create no maintenance job.
