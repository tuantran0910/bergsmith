# Browser API

BS-018.T1–T4 provide `bergsmith_coordinator::api::router(ApiState)` for
composition with `identity::routes::router`. BS-019.T1 adds the policy and
schedule configuration resources. BS-020.T1 composes both into the coordinator
`serve` runtime, which serves `/api/v1`, `/auth`, and the built UI on one
origin. Supply the same database and `SessionConfig` to both. `ApiState` also
carries a `CatalogPreviewer` built from the configured storage profiles and
planner configuration, because previews read table metadata through the
catalog, and a `JobEventHub` that admits and wakes job event streams. Runbook:
[serving](serving.md).

The contract is [OpenAPI 3.1](../../crates/coordinator/src/api/openapi.json), also
available at `GET /api/v1/openapi.json` without a session. The document is
prepared once per router with the configured session cookie name. Resource
requests use the HttpOnly session cookie issued through `/auth`; there is no
caller-selected organization or actor. Mutations echo the readable CSRF cookie
as `x-csrf-token`. All responses use `Cache-Control: no-store`.

| Endpoint | Behavior |
|---|---|
| `GET /api/v1/openapi.json` | Public OpenAPI 3.1 contract; no session required |
| `GET /api/v1/catalogs`, `GET /api/v1/catalogs/{id}` | Authorized catalog summaries; detail also includes settings for connection managers |
| `POST /api/v1/catalogs` | Create a connection under organization-wide `connections:manage`; 201 |
| `PATCH /api/v1/catalogs/{id}` | Edit, enable, or disable under connection-scoped `connections:manage`, using `expected_revision`; 409 for stale revisions or routing edits while jobs depend on the connection |
| `POST /api/v1/catalogs/{id}/check` | Read what the connection's catalog advertises and record the answer; requires connection-scoped `connections:manage` |
| `GET /api/v1/tables`, `GET /api/v1/tables/{id}` | Authorized canonical table identities |
| `GET /api/v1/jobs`, `GET /api/v1/jobs/{id}` | Authorized job state, revision, requester and their display name, the table's caller-visible aliases, cancellation/publication boundary, retry link |
| `POST /api/v1/jobs` | Idempotent durable submission; returns 202 with job and operation IDs and Location |
| `POST /api/v1/jobs/{id}/cancel` | Record a cancellation request and report what it achieved |
| `POST /api/v1/jobs/{id}/retry` | Create a fresh-plan retry of a terminal job; 202 with the new job |
| `POST /api/v1/previews` | Assess a table without creating work |
| `GET /api/v1/audit` | Organization audit history with each actor's current display name; requires organization-wide `audit:read` |
| `GET /api/v1/workers` | Current worker sessions and their lease status; requires organization-wide `workers:manage` |
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

## Catalog configuration

Create accepts `name`, `uri`, optional `warehouse`, and optional
`credential_ref`. The name uses lowercase letters, digits, underscores, and
hyphens, starts with a letter or digit, and is at most 63 characters. The URI
must be HTTP(S), with no user information, query, or fragment; credentials
stay outside the API as references. Reference identifiers contain only ASCII
letters, digits, `/`, `_`, `-`, and `.`, at most 200 bytes. Creating a
connection does not check catalog capabilities or certify write support.

PATCH accepts those fields plus `enabled` and a required `expected_revision`.
Omitted fields keep their values; null clears `warehouse` or `credential_ref`.
Only supplied fields are validated, so legacy settings do not prevent renaming
or disabling a connection. A PATCH with no editable fields returns 400.
Each change increments the revision and commits an audit record atomically.
An administrator reading the connection detail receives its settings. Other
inventory readers, and all list responses, receive the summary with enabled
state, revision, and timestamps, without the settings. Detail responses strip
URI user information, query, and fragment from legacy connections; an
unparseable legacy URI is omitted.

PATCH also accepts the connection's operating bounds:
`max_execution_seconds` (1 to 86 400, default 180),
`max_unresolved_publication_seconds` (1 to 2 592 000, default 3 600), and
`maintenance_enabled` (default true). A value outside its range returns 400 and
changes nothing. The bounds are part of the connection's configuration, so they
are revision-checked and audited like the rest.

The execution budget bounds a whole job, not one attempt. The job's plan
carries a deadline, counted from the moment the plan is recorded: a grant and
every later heartbeat cap the attempt at it, and a job whose budget is spent
starts no further work, fences a renewal, and ends with
`ExecutionBudgetExceeded`. A job created before the budget existed has no
deadline and keeps the worker's own lease. A publication that was already
authorized is never disturbed by the deadline.

The detail reports the bounds under `settings`, the effective storage profile
under `storage_profile`, and the connection's unresolved publications under
`unresolved_publications`. The profile is read-only: it names the backends
that serve the connection and whether the connection has an entry of its own
(`"source":"connection"`) or uses the default set (`"source":"default"`). An
endpoint is reported only when it is an HTTP(S) URL with a host, without user
information, query, or fragment; an opaque or unparseable value is omitted.
The publications report counts what is unresolved and whether the oldest is
past the connection's own bound; it never resolves one.

Disabling stops new previews and execution admissions. Pausing maintenance
stops new maintenance work only: previews, reads, and publication recovery
continue. A permission must hold on a connection that allows maintenance, so a
requester who may only use a paused connection has no usable route. Due
schedules record `MaintenancePaused` without creating work while maintenance is
paused, and `ConnectionDisabled` when no enabled alias their service identity
may use. Queued jobs wait; admitted jobs and publication recovery continue. No
alias or job is deleted. Endpoint, warehouse, and credential-reference edits
return `catalog_connection_in_use` while any nonterminal job belongs to a table
registered through the connection. Resolve those jobs before changing routing;
disabling remains available.

## Capability check

`POST /api/v1/catalogs/{id}/check` reads `GET /v1/config` on the connection's
catalog, records the answer with the time of the check, and returns it. It
requires `connections:manage` for that connection. The check is diagnostic: it
never changes the connection's configuration, never moves its revision, and
records a `catalog.checked` audit record naming the requirements that are not
met.

Each requirement is `met`, `missing`, or `unknown`:

| Requirement | How it is decided |
|---|---|
| `list_tables`, `load_table`, `commit_table` | The endpoint the catalog advertises. A catalog that advertises no endpoints serves the specification's default set, so all three are `met`. |
| `credentials_endpoint` | Whether the catalog advertises the table credentials endpoint. Optional. |
| `idempotency_key_lifetime_seconds` | The advertised lifetime in seconds, as a decimal string, or null. Optional. A decimal string keeps the advertised value exact: a lifetime may be larger than a browser counts. |
| `vended_credentials` | Always `unknown`: the catalog configuration cannot show it, and loading a table proves it. |

The answer is bound to the settings it describes. If the connection's routing
changes while the catalog is answering, the answer describes a configuration
that no longer exists, so it is not recorded and the check returns 409
`catalog_connection_changed`; run it again. For the same reason, a recorded
answer is not shown in `GET /api/v1/catalogs/{id}` after a routing edit.

A check is bounded four times. The catalog's answer may not exceed 64 KiB, the
check is given up after 10 seconds, the connection after 5, and one catalog
request after 30. The coordinator runs at most 8 checks at once and at most 2
for one requester; a check that cannot start returns 503
`capability_check_capacity_exhausted`, and one whose answer is too large
returns `catalog_response_too_large`. A catalog that cannot be reached leaves
no record and returns 503 `catalog_unavailable`.
A connection that names an external credential cannot be checked until the
runtime resolves credential references; the check returns
`catalog_credential_unavailable` rather than authenticating as anything else,
and a catalog that requires authentication answers `catalog_access_denied`.

## Catalog discovery

`GET /api/v1/catalogs/{id}/namespaces` lists the namespaces a connection's
catalog holds under a parent (`?parent=a&parent=b`), or the top level without
one. `GET /api/v1/catalogs/{id}/tables?namespace=db` lists one namespace's
tables and marks the ones the connection already has an alias for, with the
alias revision a removal names. Both require `connections:manage` for the
connection, and a namespace is repeated parameters, never a dotted path.

Both read one page. `limit` (1–200, default 50) is the page size asked of the
catalog; a catalog that ignores page sizes may answer with more items, up to
1000. `next_cursor` carries the catalog's own continuation token and is bound
to the caller, the connection, and the listing, so it only continues the
listing it was issued for; `null` means the catalog has no more. `skipped`
counts identifiers the catalog returned that Bergsmith cannot address (an
empty level, a level or name over 255 bytes, a control character, more than 16
levels); they are never shown as registrable. A page holding more than 1000
items or 1 MiB of identifiers returns 500 `catalog_page_too_large`, and a
catalog token too long to carry in a cursor returns 500
`catalog_token_too_large` instead of a continuation the next request would
refuse.

The catalog's connect and request timeouts bound each call, and the
coordinator runs at most 4 discovery operations at once; one that cannot start
returns 503 `catalog_discovery_capacity_exhausted`. A catalog that does not
advertise namespace listing returns 409 `catalog_unsupported`, a missing
namespace 404 `catalog_namespace_not_found`, a table the catalog places
outside the requested namespace 409 `catalog_namespace_mismatch`, and an
unreachable catalog 503 `catalog_unavailable`.

`POST /api/v1/catalogs/{id}/registrations` accepts
`{"namespace":["db"],"table":"events"}` to register one table or
`{"namespace":["db"]}` to register the whole namespace, and answers 201 with
the registered names, or 200 when every name was registered already. The
catalog is read before anything is written, and the aliases and their
`catalog.tables_registered` audit record commit together, so a request records
every name it was given or none. A namespace registration reads the namespace
page by page before it writes, so a namespace that spans pages is one request.
One request records at most 100 tables; a larger namespace returns 409
`registration_too_large`, and one holding a name Bergsmith cannot address
returns 409 `registration_incomplete`. A name the catalog does not serve
returns 404 `catalog_table_not_found`, and one that now reaches a different
table returns 409 `table_identity_conflict`. The aliases are bound to the
routing they were read with: an edit that lands while the catalog is being
read leaves nothing recorded and returns 409 `catalog_connection_changed`, so
the request is run again.

`DELETE /api/v1/catalogs/{id}/registrations?namespace=db&table=events&revision=7`
removes the alias row the caller observed and returns the canonical table it
used to reach. The revision is what the table browse reported, or the alias
revision on `GET /api/v1/tables/{id}` when the catalog no longer lists the
name: a name that was removed and registered again carries a new one, so a
delayed removal answers 409 `revision_conflict` instead of deleting the
replacement. The table and its other names stay, and an unknown name returns
404. Every name of a table with nonterminal jobs is kept, so the removal
returns 409 `table_in_use` until the work ends.

## Previews
`POST /api/v1/previews` accepts `{"table_id":"<canonical table UUID>"}` and
requires `jobs:submit` on that table. It creates no job, plan, task, output,
idempotency key, or audit record; denials are audited. Unknown tables return 404
and inaccessible ones 403 before any catalog request.

The catalog name is chosen only among enabled connections of the table's aliases
whose connection the caller's grant covers, in canonical order. If every usable
alias is disabled, the preview returns 409 `catalog_connection_disabled` before
catalog I/O. A connection-scoped grant therefore
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
seconds. The `serve` runtime starts `JobEventHub::listen`, which holds one dedicated
PostgreSQL connection per process in addition to the request pool.

## Policies and schedules

A policy is the reusable "what": an operation (`Compact` in this release), a
target file size between 1 MiB and 1 GiB, a minimum input-file count between
2 and 1000, and `max_input_bytes`, the most input one run may rewrite. The run
budget is between 1 MiB and 100 GiB, defaults to 100 GiB, and must be at least
the target file size. It is applied at planning together with the coordinator's
global admission cap, so the effective limit is the smaller of the two. A policy
names no table.

A schedule binds one policy to one canonical table, a five-field cron expression
in an IANA time zone, an optional daily maintenance window, and a service
principal. The window is a local-time range in the schedule's zone:
`start_minute` is inclusive, `end_minute` exclusive, both are minutes after
midnight in `0..=1439`, and a start later than the end wraps past midnight. A
window gates the start: admission checks the instant at which the job would be
created, and an occurrence evaluated outside the window is skipped with reason
`OutsideWindow`; the skipped span is recorded once and the next run waits for
the next opening. The window does not limit how long a started run may take. A
policy runs at most once per table; changing the timing updates that schedule
instead of adding another.

`POST /api/v1/policies` accepts `name` and optional `operation`,
`target_file_size_bytes`, `min_input_files`, and `max_input_bytes`, defaulting
to 256 MiB, five files, and 100 GiB. `PATCH` accepts the `revision` the caller
read plus any of `name`, `operation`, `target_file_size_bytes`,
`min_input_files`, `max_input_bytes`, and `enabled`.
`POST /api/v1/schedules` accepts `policy_id`, `table_id`, `cron_expression`,
`time_zone`, an optional `window` object with `start_minute` and `end_minute`,
and `service_principal_id`; `PATCH` accepts `revision` plus any of
`cron_expression`, `time_zone`, `window`, and `enabled`. For `window`, an absent
field keeps the stored value, an explicit `null` clears it, and an object
replaces it. Changing the window recomputes a future next-due cursor so a skip
under the old window cannot hide newly eligible runs; an overdue cursor is still
evaluated under the new window. A schedule's policy and table cannot change.

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
time zone return 400 `invalid_cron_expression` and `invalid_time_zone`. A window
whose bounds are equal or outside one day returns 400
`invalid_maintenance_window`. Out-of-range policy parameters return 400
`invalid_policy_parameters`, a run budget below the target file size returns
`run_budget_below_target_file_size`, an unknown operation returns
`invalid_policy_operation`, and a service principal that is not a service
account with a current `jobs:submit` grant for the table returns 400
`service_identity_not_eligible`.

Admission persists an occurrence before creating a job, coalesces missed
occurrences and overlapping runs on one table, and rechecks the enabled policy
and service grant. A disabled policy also stops a scheduled job before it
publishes. Disabling only a schedule stops future admission; cancelling an
existing job remains an explicit command. See
[Scheduling and reconciliation](scheduling.md) for occurrence and window
semantics.

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
