# Browser API

BS-018.T1–T4 provide `bergsmith_coordinator::api::router(ApiState)` for
composition with `identity::routes::router`. BS-019.T1 adds the policy and
schedule configuration resources. Supply the same database and
`SessionConfig` to both. `ApiState` also carries a `CatalogPreviewer` built from
the configured storage profiles and planner configuration, because previews read
table metadata through the catalog, and a `JobEventHub` that admits and wakes
job event streams. The coordinator binary currently provides
migration and administrator bootstrap commands; it does not yet start an HTTP
server.

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
| `POST /api/v1/jobs/{id}/cancel` | Record a cancellation request and report what it achieved |
| `POST /api/v1/jobs/{id}/retry` | Create a fresh-plan retry of a terminal job; 202 with the new job |
| `POST /api/v1/previews` | Assess a table without creating work |
| `GET /api/v1/audit` | Organization audit history; requires organization-wide `audit:read` |
| `GET /api/v1/policies`, `GET /api/v1/policies/{id}` | Maintenance policies; requires organization-wide `schedules:manage` |
| `POST /api/v1/policies` | Create a policy; 201 with the stored row |
| `PATCH /api/v1/policies/{id}` | Edit or disable a policy using the revision read; 409 when it is stale |
| `GET /api/v1/schedules`, `GET /api/v1/schedules/{id}` | Maintenance schedules; requires organization-wide `schedules:manage` |
| `POST /api/v1/schedules` | Bind a policy to one table with a cron expression, time zone, and service identity; 201; requires organization-wide `schedules:manage` |
| `PATCH /api/v1/schedules/{id}` | Edit a schedule's timing or enabled state using the revision read; requires organization-wide `schedules:manage` |

Submit `{"table_id":"<canonical table UUID>"}` with a nonempty, printable ASCII
`Idempotency-Key` of at most 255 bytes. Unknown body fields are rejected, so a
request cannot override the actor or attach a retry target. Identical replay
returns the original operation. A changed body or actor with that key returns
409; use a fresh key for new work. Keys are scoped to the organization.
Submission and its audit record commit together; an audit failure creates no
job or idempotency binding.

Submission creates a `Planning` job. A 202 response means only that the request
is durable. `ReadyToCommit` means execution outputs are ready; `Succeeded` means
publication is confirmed. `CommitOutcomeUnknown` must be resolved from
publication evidence. `ReplanRequired` requires a new job and fresh plan; old
accepted outputs cannot be transferred. T3 supplies SSE; this task does not
certify M4.

## Previews

`POST /api/v1/previews` accepts `{"table_id":"<canonical table UUID>"}` and
requires `jobs:submit` on that table. It creates no job, plan, task, output,
idempotency key, or audit record; denials are audited. Unknown tables return 404
and inaccessible ones 403 before any catalog request.

The catalog name is chosen only among the table's aliases whose connection the
caller's grant covers, in canonical order. A connection-scoped grant therefore
never causes a request through another connection. Every response is bounded:
at most 1,000 bins are returned, with `bins_total` and `bins_truncated` telling
the client what was omitted, and at most 100,000 manifest entries are read. At
most eight previews run at once per process and two per requester; excess
requests return 503 with `preview_capacity_exhausted` and `RetrySameRequest`.

The response describes what a job would be built from:

| Field | Meaning |
|---|---|
| `outcome` | `planned`, `no_op`, or `rejected` |
| `no_op_reason` | `no_snapshot` or `no_beneficial_work` when nothing would run |
| `baseline` | `snapshot_id` (a decimal string, so browsers do not round it) and `sequence_number` assessed; null without a `main` snapshot |
| `rejections` | Typed refusals: `format_version`, `sorted_table`, `encrypted`, `write_format`, `non_parquet_file`, `equality_key`, `void_spec_equality_delete`, each with a human `detail` and the variant fields that apply |
| `candidates` | Data files selected for compaction and their bytes |
| `skipped` | Non-candidates by `outside_selection`, `too_large`, or `at_target_size` |
| `bins` | Candidate groups with their partition key, input tally, `files_removed`, and an admission disposition (`admitted`, `no_benefit`, or a `deferred_*` limit) |
| `bins_total`, `bins_truncated` | How many bins the assessment held, and whether `bins` omitted any |
| `estimate` | Admitted `tasks`, `files`, `bytes`, `files_removed`, and expected `duration_millis` |

Baseline metadata locations, table locations, credentials, and connection
settings are not returned. Per-file rejection `path` values are, because they
are what explains a refusal.

A preview loads the current metadata from that catalog; nothing is cached except
the catalog client for the connection, which is reused while its URI and
warehouse are unchanged. Catalogs are authenticated from stored connection
settings only. A connection that names an externally held credential reference
currently fails with `catalog_credential_unavailable` and `PermanentFailure`,
because the runtime does not resolve credential references yet. Storage is
selected by the table location's backend from the profile configured for that
connection; a missing profile fails with `storage_profile_unavailable`. If the
catalog reports a different `table-uuid` than the canonical identity, the
request fails with 409 `table_identity_changed`; refresh inventory and retry.
The identity is checked immediately after each metadata load, before a storage
profile is selected or credentials are requested, so a replacement table cannot
surface as a storage or credential error.
Previews always assess the whole table; partition selection is not accepted yet.

Catalog failures that need operator action are permanent, not retryable: a
rejected coordinator credential (`catalog_access_denied`), a catalog without the
required endpoints (`catalog_unsupported`), unusable or absent vended credentials
(`catalog_credentials_unavailable`), an invalid catalog URL
(`catalog_connection_invalid`), and other catalog 4xx responses
(`catalog_request_rejected`). Only transport failures, timeouts, throttling,
undecodable bodies, and upstream 5xx responses are 503 `RetrySameRequest`.
Server-side preview failures are logged with the table, the reason, a sanitized
error kind, and an upstream HTTP status when there was one — never the
catalog's message, URLs, table locations, or Iceberg error text.

## Job commands

`POST /api/v1/jobs/{id}/cancel` requires `jobs:cancel` on the job's table and
accepts an optional `{"reason": "..."}` of at most 500 characters. Authorization,
the cancellation, and its audit record commit together, so a request the audit
cannot record leaves the job unchanged. The request is idempotent and monotonic:
repeating it changes nothing and returns the same outcome, and the first
requester and reason are kept. The audit records the reason that was actually
persisted. The response reports the job's `state`, `revision`,
`cancellation_requested`, `publication_started`, and a `cancellation` outcome:

| `cancellation` | Meaning |
|---|---|
| `cancelling` | Cancellation won before send authorization; the job is `Cancelling` |
| `too_late_publication_started` | A commit request was already authorized and may publish |
| `publication_unresolved` | The commit outcome is unknown; no abort is assumed |
| `too_late_committed` | The job published |
| `cancelled` | The job already ended cancelled |
| `already_ended` | The job ended another way without publishing; nothing changed |

A late or already-ended request is a truthful 200 rather than an error.

`POST /api/v1/jobs/{id}/retry` requires `jobs:retry` on the table and a required
`Idempotency-Key`. It checks authorization before reporting the source's state,
so an unauthorized caller cannot learn it, and again inside the creation
transaction. Only a terminal source that did not succeed may be retried: a
non-terminal source returns 409 `job_not_terminal`, and a succeeded source
returns 409 `job_already_succeeded`. The response is a 202 with the new job,
operation ID, Location, and `retry_of_job_id`. The new job starts in `Planning`;
no plan, output, grant, or accepted result of the source is reused, and a fresh
key creates another independent retry. Job creation, the idempotency binding,
and the `job.retry_requested` audit record commit together.

## Job events

`GET /api/v1/jobs/{id}/events` streams one job's history as server-sent events
(`text/event-stream`). It requires `jobs:read` on the job's table, checked
exactly as for `GET /api/v1/jobs/{id}` before the stream starts; unknown and
inaccessible jobs are both 404.

Each event's `id` is the job's sequence (1, 2, 3, ...), its `event` is one of
`created`, `transitioned`, `cancellation_requested`, or
`publication_authorized`, and its `data` is a JSON object with `sequence`,
`kind`, `from_state`, `to_state`, `state_revision`, `reason`, `actor_id`, and
`occurred_at`. `reason` is the typed state reason of a transition and is null
otherwise; an operator's cancellation text is never streamed.

Sequences are assigned while the job row is locked, so they commit in order.
To resume, send the last ID received as `Last-Event-ID` (a browser
`EventSource` does this automatically) or as `?after=`; the header wins. The
stream then contains every later event exactly once. Values must be
non-negative decimals; anything else is 400.

The stream sends a keep-alive comment every 15 seconds. It ends, and the client
should reconnect with its last ID, when:

- the event that reaches a terminal state has been sent (a later reconnect
  ends immediately);
- the session is revoked, expires, or goes idle, or the caller loses access to
  the job (checked at most every 30 seconds; the check does not keep the
  session alive);
- the stream has been open for one hour.

Each process serves at most 256 streams, and 8 per principal; beyond that the
request returns 503 `event_stream_capacity_exhausted` with `RetrySameRequest`.
Delivery is woken by a PostgreSQL notification on the `bergsmith_job_events`
channel; if notifications are lost, streams still poll the ledger every 2
seconds. A runtime that serves the router starts `JobEventHub::listen`, which
holds one dedicated PostgreSQL connection per process in addition to the request
pool.

## Policies and schedules

A policy is the reusable "what": an operation (`Compact` in this release), a
target file size between 1 MiB and 1 GiB, and a minimum input-file count between
2 and 1000. It names no table. A schedule binds one policy to one canonical
table, a five-field cron expression in an IANA time zone, and a service
principal. A policy runs at most once per table; changing the timing updates
that schedule instead of adding another.

`POST /api/v1/policies` accepts `name` and optional `operation`,
`target_file_size_bytes`, and `min_input_files`, defaulting to 256 MiB and five
files. `PATCH` accepts the `revision` the caller read plus any of `name`,
`operation`, `target_file_size_bytes`, `min_input_files`, and `enabled`.
`POST /api/v1/schedules` accepts `policy_id`, `table_id`, `cron_expression`,
`time_zone`, and `service_principal_id`; `PATCH` accepts `revision` plus any of
`cron_expression`, `time_zone`, and `enabled`. A schedule's policy and table
cannot change.

Edits are optimistic. A request whose `revision` is stale returns 409
`revision_conflict` and changes nothing. `POST` on an existing policy name or an
existing policy/table pair returns 409 `resource_already_exists`. Every schedule
and policy operation requires organization-wide `schedules:manage`; a grant
scoped to one table or connection does not administer organization
configuration. Every mutation, and its `policy.created`, `policy.updated`,
`schedule.created`, or `schedule.updated` audit record, commits in one
transaction.

Timing follows the plan's DST rules: a local time skipped by spring-forward is
not an occurrence, and a local time repeated by fall-back runs once at the
earlier instant. The time zone must be `UTC` or a regional IANA name containing
a `/`; abbreviations such as `EST` or `HST` are rejected because they pin a
fixed offset and would not follow daylight saving. Cron syntax and an unknown
time zone return 400 `invalid_cron_expression` and `invalid_time_zone`. Out-of-range policy
parameters return 400 `invalid_policy_parameters`, an unknown operation returns
`invalid_policy_operation`, and a service principal that is not a service
account with a current `jobs:submit` grant for the table returns 400
`service_identity_not_eligible`.

These resources define configuration only. Occurrence persistence,
deduplication, coalescing, and admission are BS-019.T2; windows and budgets are
BS-019.T3. Disabling a schedule or policy stores the change now, and enforcement
at admission follows with T2.

## Pagination, errors, and authentication

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
ledger, catalog, or storage failure returns 503 with `RetrySameRequest`; retry
using the same request (and submission key). A permanent configuration gap, such
as an unresolvable credential reference or a missing storage profile, returns 500
with `PermanentFailure` and an operator action. An unexpected server failure
returns 500 with `PermanentFailure` and is logged for operators; do not retry it
unchanged. Unauthorized sessions return 401, denied commands return 403, and
inaccessible or unknown resource IDs return 404. A catalog that no longer has
the table returns 404 `catalog_table_missing`, while an unknown canonical table
returns 404 `resource_not_found`. Credential references and catalog connection
settings are excluded from responses. Artifact access and configuration writes
are outside this resource surface. The trust boundary for catalog connections is
deployment configuration: a stored URI is contacted as given, so connection
creation must remain an administrator operation and egress should be restricted
at the network layer.

The browser application that uses this contract is described in
[the web UI guide](web-ui.md). Its client is generated from the OpenAPI
document; regenerate it with `npm --prefix web run generate` whenever
`openapi.json` changes.

Run `make services-up` followed by
`DATABASE_URL=postgres://bergsmith:bergsmith@127.0.0.1:55432/bergsmith cargo test -p bergsmith-coordinator --test api`.
