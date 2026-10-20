# Browser API

BS-018.T1 provides `bergsmith_coordinator::api::router(ApiState)` for composition
with `identity::routes::router`. Supply the same database and `SessionConfig` to
both. The coordinator binary currently provides migration and administrator
bootstrap commands; it does not yet start an HTTP server.

The contract is [OpenAPI 3.1](../../crates/coordinator/src/api/openapi.json), also
available at `GET /api/v1/openapi.json` without a session. The document is
prepared once per router with the configured session cookie name. Resource
requests use the HttpOnly session cookie issued through `/auth`; there is no
caller-selected organization or actor. Mutations echo the readable CSRF cookie
as `x-csrf-token`. All responses use `Cache-Control: no-store`.

| Endpoint | Behavior |
|---|---|
| `GET /api/v1/openapi.json` | Public OpenAPI 3.1 contract; no session required |
| `GET /api/v1/catalogs`, `GET /api/v1/catalogs/{id}` | Authorized catalog identity summaries |
| `GET /api/v1/tables`, `GET /api/v1/tables/{id}` | Authorized canonical table identities |
| `GET /api/v1/jobs`, `GET /api/v1/jobs/{id}` | Authorized job state, revision, requester, cancellation/publication boundary, retry link |
| `POST /api/v1/jobs` | Idempotent durable submission; returns 202 with job and operation IDs and Location |
| `GET /api/v1/audit` | Organization audit history; requires organization-wide `audit:read` |
| `POST /api/v1/previews` | Explicit 501 until BS-018.T2 |
| `GET /api/v1/schedules`, `GET /api/v1/policies` | Explicit 501 until BS-019; requires organization schedule administration |

Submit `{"table_id":"<canonical table UUID>"}` with a nonempty, printable ASCII
`Idempotency-Key` of at most 255 bytes. Unknown body fields are rejected, so a
request cannot override the actor or attach a retry target. Identical replay
returns the original operation. A changed body or actor with that key returns
409; use a fresh key for new work. Keys are scoped to the organization.
Submission and its audit record commit together; an audit failure creates no
job or idempotency binding.

Submission creates a `Planning` job. T2 supplies preview/planning execution and
job commands; T3 supplies SSE. A 202 response means only that the request is
durable. `ReadyToCommit` means execution outputs are ready; `Succeeded` means
publication is confirmed. `CommitOutcomeUnknown` must be resolved from
publication evidence. `ReplanRequired` requires a new job and fresh plan; old
accepted outputs cannot be transferred. This task does not certify M4.

Lists accept `limit` (default 50, maximum 100) and an opaque `cursor`. Follow
`next_cursor` until it is null. The cursor binds the resource, principal, and
organization. Visibility is checked on every page; revocation immediately
changes what a continuation can see. Results use ascending UUID order, or
ascending durable event ID for audit. Item, first-page, and continuation reads
use separate static queries with direct identifier predicates. Audit IDs are
decimal strings to preserve 64-bit precision in browser clients. Lists are live
views rather than snapshots. Audit appends are serialized per organization, so a
tenant's continuation cannot skip a late commit; organization-less events are
not listed.

A request with no session credential, a malformed one, or a well-formed token
that was never issued returns 401. It is logged, but not written to the audit
log: such a credential is unattributable, and persisting every request would let
unauthenticated clients grow the append-only audit log. Rejected sessions that
map to a known organization and CSRF failures are audited on a best-effort
basis. An audit outage never changes the 401 or 403 response.

Errors contain `reason`, `disposition`, and `operator_action`. A transient
ledger outage returns 503 with `RetrySameRequest`; retry using the same
submission key. An unexpected server failure returns 500 with
`PermanentFailure` and is logged for operators; do not retry it unchanged.
Unauthorized sessions return 401, denied commands return 403, and inaccessible
or unknown resource IDs return 404. Credential references and catalog connection
settings are excluded from responses. Artifact access and configuration writes
are outside this initial resource surface.

Run `make services-up` followed by
`DATABASE_URL=postgres://bergsmith:bergsmith@127.0.0.1:55432/bergsmith cargo test -p bergsmith-coordinator --test api`.
