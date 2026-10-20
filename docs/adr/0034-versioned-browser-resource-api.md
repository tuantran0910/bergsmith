# 0034. Versioned browser resource API

- Status: Accepted
- Date: 2026-10-20
- Plan reference: BS-018.T1, public API and UI

## Context

The coordinator has durable identity, inventory, and execution services but no
browser resource API. Its first API must preserve scoped authorization and
provide a contract for the generated UI client without inventing execution or
schedule behavior assigned to later tasks.

## Decision

- Expose session-authenticated resources under `/api/v1`. Derive organization
  and actor from the session and require the existing CSRF token for mutations.
  Merge this router with the identity router using the same session policy.
  Build protected routes separately from the public contract route so a new
  resource cannot become public by registration order.
- A missing, malformed, or never-issued credential is answered with 401 and
  logged, but not written to the audit log: it is unattributable, and persisting
  every such request would let unauthenticated clients grow the append-only log
  and contend with real audit writers. A rejected session that maps to a known
  organization, and a CSRF failure, are audited on a best-effort basis; an audit
  failure never changes the 401 or 403 response.
- Publish the checked-in OpenAPI 3.1 document at `/api/v1/openapi.json`. It is
  public and contains no deployment configuration. Prepare it once when the
  router is built, with the configured session cookie name. Test resource
  response fields against its schemas; BS-018.T4 generates the client from this
  document. The public route documents its method-not-allowed response.
- Read catalog, table, and job summaries with grants filtered in SQL before the
  keyset bound and limit. Sort by UUID. Use separate static queries for item
  lookup, the first page, and a continuation so the identifier bound is a direct
  index predicate rather than a nullable range. Cursor versions bind the
  continuation to the organization, principal, and resource. Cursors convey no
  authority; each page checks current grants. Lists default to 50 and accept
  1–100 items. Unknown and inaccessible item identifiers both return 404. Do not
  return credentials, credential references, connection settings, or plan
  artifacts.
- Audit reads require an organization-wide audit permission. Page in ascending
  durable ID order and serialize bigint IDs as decimal strings for JavaScript.
  Serialize audit appends per organization so a tenant's cursor cannot skip a
  late commit; organization-less events are not listed and take no lock.
- `POST /api/v1/jobs` accepts only a canonical table ID and a required
  `Idempotency-Key`. Bind the key to the typed request and actor within the
  organization. Write the Planning job, creation event, key, and audit atomically.
  A concurrent replay rolls back its losing job. Return the same 202 operation
  identifier and Location on replay, even after the job has advanced.
- Recheck submission permission in the mutation transaction. Admission and
  publication retain their existing authorization checks. Submission itself
  neither admits work nor performs catalog I/O.
- Return stable error reasons, failure dispositions, and operator actions.
  Use `RetrySameRequest` for transient ledger unavailability, `PermanentFailure`
  with a 500 internal error for unexpected server failures, and log both
  server-side without request bodies or credentials. Send `Cache-Control:
  no-store` on every API response.
- Reserve preview submission and schedule/policy list endpoints with explicit
  authenticated 501 responses. Preview execution and commands belong to
  BS-018.T2; schedule/policy models and execution belong to BS-019. Do not create
  placeholder rows or claim these capabilities are implemented. Configuration
  mutation endpoints, when added, must require optimistic revisions.

## Consequences

Resource reads and durable submission are independently usable and testable.
The job operation ID is its job ID; a Planning job is pending work, not an
execution or publication success. Execution wiring and server startup remain
follow-up work; this task provides a composable router, matching the existing
identity routes. The API documentation records these limits explicitly.

Anonymous credential failures are observable in logs rather than the audit log.
Denial auditing is best effort, so an audit outage cannot turn a safe 401 or 403
into a retryable server error. Audit ordering and contention are organization
scoped.

Cursors are versioned encodings, not signatures. Editing a cursor can change a
continuation bound but cannot widen visibility. Pagination is not a snapshot:
new resources and grant changes can affect subsequent pages. Global audit
events with no organization are intentionally excluded from tenant audit reads.
