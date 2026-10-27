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
| `GET /api/v1/principals`, `GET /api/v1/principals/{id}` | The organization's users and service accounts, active and deactivated alike; requires organization-wide `grants:manage` |
| `POST /api/v1/principals` | Provision a user for an issuer and subject with a display name; 201; requires organization-wide `grants:manage` |
| `PATCH /api/v1/principals/{id}` | Deactivate (`{"active":false}`) or reactivate (`{"active":true}`) a principal; 409 `last_administrator` for the last active administrator |
| `GET /api/v1/grants` | Active grants in ascending identifier order, optionally filtered by `principal_id`, `catalog_connection`, or `table`; the filter's scope requires `grants:manage`. A connection page includes the grants on its tables |
| `POST /api/v1/grants` | Grant a role over the organization (`"scope": {}`), one connection, or one table; 201; requires `grants:manage` over that scope; 409 `grant_already_exists` for an identical active grant |
| `DELETE /api/v1/grants/{id}` | Revoke an active grant, keeping the row for audit; requires `grants:manage` over its scope; 409 `last_administrator` when it is the last active organization-wide administrator grant |
| `GET /api/v1/tables`, `GET /api/v1/tables/{id}` | Authorized canonical table identities, each with the newest metadata-only statistics summary or `null` when unsampled, and its evaluated health (`state` plus the sample and confirmation instants; `state` is `null` when unassessed). The list accepts `health` to keep one exact state (`healthy`, `warning`, `critical`, `unassessed`) and `sort=health` to order the worst state first with ties by id; either evaluates every visible table from its newest sample and the thresholds of its enabled policies, so the cost grows with the inventory rather than the page. A cursor belongs to the query shape that produced it, and a health-filtered cursor resumes after the health sort key |
| `GET /api/v1/tables/{id}/statistics` | The table's newest sample and a cursor page of its history; `since` filters by instant; counts and bytes are decimal strings; partition values appear only for callers allowed to submit jobs on the table; 404 for unknown or invisible tables |
| `GET /api/v1/tables/{id}/definition` | The table's current schema, every retained partition spec, its default sort order, and the allow-listed properties (`DEFINITION_PROPERTIES`) that are set; metadata only, so no data or delete file is read and no partition value, metadata location, credential, or connection setting is returned; 404 for unknown or invisible tables |
| `GET /api/v1/tables/{id}/snapshots` | The table's recorded commits, newest first, each with its operation, the change counts its summary records, the committing engine's summary properties, and the Bergsmith job that published it; `hours` (1–168, default 24) sizes the commit-per-hour cadence ending with the current hour; per-partition counters appear only for callers allowed to submit jobs on the table; counts and IDs are decimal strings; 404 for unknown or invisible tables |
| `GET /api/v1/tables/{id}/recommendations` | Health findings from the table's newest metadata-only sample and the action each finding recommends. The table's state is the worst evaluation across its enabled policies' threshold sets, and each finding names the source of its thresholds (`kind: defaults`, or `kind: policy` with the policy id and name); a disabled policy or schedule contributes no set. Findings need inventory visibility only, so a viewer can explain a state; the catalog assessment behind an actionable recommendation needs `jobs:submit`, and `assessment` says what to expect (`included`, `requires_jobs_submit`, or `not_applicable` for an advisory recommendation). An actionable finding (small files) carries the prefilled preview and the planner's estimate from the current table (`outcome`, `estimate`); an advisory finding carries the reason it cannot run yet (delete pressure needs BS-031's delete cleanup, partition skew needs partition targeting, snapshot and manifest work is BS-031, stale statistics resolves on the collector's interval). Visibility is checked first, so a table outside the caller's inventory is 404. A table without a sample, or with a sample predating the health counters, returns `state: null` with no findings or recommendations and no catalog request; counters are decimal strings |
| `GET /api/v1/tables/{id}/maintenance` | The table's terminal jobs, newest first, each with the files and bytes its accepted tasks rewrote, the delete files they applied, and its elapsed time, plus totals over the range; `since` (RFC 3339, default thirty days ago) sets the range on the first page and the cursor carries it afterwards; accepted output is partitioned by outcome (`output_*` published, `discarded_*` after a conflict, `abandoned_*` from stopped jobs) so no byte is counted twice, and delete files count once per job; requires `jobs:read` on the table and returns 404 for unknown or invisible tables; counters are decimal strings |
| `GET /api/v1/statistics/maintenance` | The same impact totals over every table the caller may read jobs on, over the range; counters are decimal strings |
| `GET /api/v1/overview` | The landing overview: terminal jobs whose transition falls in the last day, by outcome and conflict rate; the unresolved-publication count and oldest age in full plus a bounded list of jobs that need attention (unresolved publications first, aged from send authorization, then `ReplanRequired` and failed jobs, aged from their terminal transition); visible tables that were never sampled or whose newest job needs a decision; enabled schedules whose next due instant has passed, plus occurrences folded or skipped in the last day; and registered worker capacity. Job and table figures follow the caller's grants, and the schedule and worker sections are `null` when the caller may not manage them; counters are decimal strings |
| `GET /api/v1/catalogs/{id}/statistics` | Totals over the newest sample of every visible table on one connection; 404 when the connection is not visible |
| `GET /api/v1/statistics` | The same totals over every table the caller may view in the organization |
| `GET /api/v1/jobs`, `GET /api/v1/jobs/{id}` | Authorized job state, revision, requester and their display name, the table's caller-visible aliases, cancellation/publication boundary, retry link, and the `trigger` (finding kind and severity) that admitted it, or null |
| `POST /api/v1/jobs` | Idempotent durable submission of `compact` (default) or `expire_snapshots`; returns 202 with job and operation IDs and Location |
| `POST /api/v1/jobs/{id}/cancel` | Record a cancellation request and report what it achieved |
| `POST /api/v1/jobs/{id}/retry` | Create a fresh-plan retry of a terminal job; 202 with the new job |
| `POST /api/v1/previews` | Assess a table without creating work |
| `GET /api/v1/audit` | Organization audit history with each actor's current display name; requires organization-wide `audit:read` |
| `GET /api/v1/workers` | Current worker sessions and their lease status; requires organization-wide `workers:manage` |
| `GET /api/v1/policies`, `GET /api/v1/policies/{id}` | Maintenance policies; requires organization-wide `schedules:manage` |
| `POST /api/v1/policies` | Create a policy; 201 with the stored row. `health_thresholds` overrides per-finding bounds (absent findings keep the documented defaults) and `health_trigger` names the finding and severity that admit maintenance; both are optional |
| `PATCH /api/v1/policies/{id}` | Edit or disable a policy using the revision read; 409 when it is stale. A provided `health_thresholds` replaces the stored set; `health_trigger` is left alone when absent and cleared by null |
| `GET /api/v1/schedules`, `GET /api/v1/schedules/{id}` | Maintenance schedules; requires organization-wide `schedules:manage` |
| `POST /api/v1/schedules` | Bind a policy to one table with a cron expression, time zone, and service identity; 201; requires organization-wide `schedules:manage` |
| `PATCH /api/v1/schedules/{id}` | Edit a schedule's timing or enabled state using the revision read; requires organization-wide `schedules:manage` |

Principal administration requires `grants:manage` over the organization,
because the issuer and subject are the key sign-in is bound to. Deactivation is
a lifecycle change, not a deletion: the row, its grants, and its history stay,
its sessions end in the same transaction, sign-in refuses it, the inventory read
filters no longer reveal resources to it, and every admission and publication
check stops applying its grants. Running jobs are not cancelled. Reactivation
restores the same access because the grants were never removed. A repeated
deactivation or reactivation changes nothing and writes no audit record.

Grant administration follows the same rule: a caller may read and change only
grants within a scope where they hold `grants:manage`. An organization
administrator sees and manages every grant; a connection administrator manages
that connection and its tables but cannot create or revoke organization-wide
access or reach another connection; a table administrator manages that table.
A connection's page also carries the grants on its tables because those grants
apply to it. A grant names one scope, and the list cursor is bound to the
filters it was issued with. Revocation is logical: the row stays with the ID of
the principal that revoked it, and the next admission, publication, or command
stops applying it. Revoking the last active organization-wide Administrator
grant is refused with 409, because a scoped administrator cannot restore
organization administration; the check shares the organization lifecycle lock
with deactivation.

Grant documents include current `principal_name` and `scope_name` display
labels alongside the stable IDs. Table labels use an alias visible to the
caller; labels do not expose an inaccessible connection or replace IDs as keys.

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

`POST /api/v1/catalogs/check` accepts the same fields as create and checks an
unsaved draft before the browser saves it. It requires organization-wide
`connections:manage` and CSRF. The response has `checked_at` (database time)
and `capabilities`, without a catalog ID. It shares the saved check's byte,
timeout, and concurrency limits, holds no ledger connection during catalog
I/O, and creates no connection or saved report. A successful diagnostic is
audited as `catalog.checked` with a null resource ID and no routing or credential
reference values. Permission is checked again before that audit. Invalid
fields return 400; credential resolution and catalog failures use the same
typed errors as the saved check. A failed diagnostic creates no successful
check audit. The browser requires a fresh diagnostic after any draft edit.

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
state, revision, timestamps, and operating bounds, without administrator-only
routing settings or diagnostic reports. Detail responses strip
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

## Job operations

`POST /api/v1/jobs` requires `jobs:submit` on the table and an `Idempotency-Key`.
The body is `{"table_id": "<uuid>"}` for compaction or
`{"table_id": "<uuid>", "operation": "expire_snapshots"}` for snapshot
expiration; an unknown operation answers 400. Expiration also requires
`expiration.catalog_ref_safety`, because a catalog must atomically refuse to
remove a snapshot that a concurrently created branch or tag references;
otherwise the request answers 400 `expiration_ref_safety_unavailable`. A
prepared expiration whose first send was never authorized fails with
`ExpirationRefSafetyUnavailable` if the coordinator restarts without the
guarantee; an authorized send is never resent.
A submission that omits `operation` keeps the pre-expiration canonical
representation for idempotency, so a retry with an original compaction key
still replays the original job. Every job document reports
`operation` as `compact` or `expire_snapshots`, and a retry copies the source's
operation.

Snapshot expiration is metadata-only. The planner selects snapshot entries
older than both `history.expire.max-snapshot-age-ms` and the connection's
`max_execution_seconds + max_unresolved_publication_seconds` window, keeping
the current snapshot, every branch and tag head, the table's
`history.expire.min-snapshots-to-keep` per branch, and every snapshot that a
nonterminal job, an expiration plan, or an unresolved publication pins.
Nothing is removed when the selection is empty: the job ends `NoOp` with
`no_expired_snapshots`.

A successful expiration publishes the standard `remove-snapshots` update
through the same table barrier, send authorization, and audit path as a rewrite.
It deletes no data, manifest, or manifest-list files; those become orphan
candidates for the separate cleanup work. If the commit response is lost, the
job stays `CommitOutcomeUnknown` until the table shows an unchanged `main` head,
every listed snapshot absent, and every pinned snapshot present, or an operator
attests non-publication. A snapshot pinned after planning causes
`ReplanRequired` with `ExpirationProtectedSnapshot`.

## Job statistics

`GET /api/v1/jobs/{id}` includes `statistics` under the same `jobs:read`
authorization as the job. Job lists omit this detail projection. Counters and
byte sizes are decimal strings to preserve integers above the browser's exact
number range. Planned input comes from the immutable plan; rewritten input and
delete-file counts include only tasks with accepted receipts, and output files
and bytes come from their verified inventory. Shared delete files count once.
Failed or unaccepted attempts do not inflate accepted work. Sizes describe
logical files, not network traffic; averages divide these sizes by file counts.
Elapsed job time includes planning, queueing, execution, and publication, and
stops at the terminal update. Accepted output is not evidence of publication:
`publication_confirmed` is true only for a succeeded job.

The job page shows these statistics separately from execution and publication
status. Before a plan exists, estimates and averages are unavailable. A preview
is a new read of catalog metadata, so it can still recommend work after a real
job if the current files still meet the planner's criteria. Browser demo fixtures
simulate responses; a demo success does not execute workers or change a table.

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
`target_file_size_bytes`, `min_input_files`, `max_input_bytes`, and the health
conditions `health_thresholds` and `health_trigger`, defaulting to 256 MiB,
five files, and 100 GiB. `PATCH` accepts the `revision` the caller read plus
any of `name`, `operation`, `target_file_size_bytes`, `min_input_files`,
`max_input_bytes`, `health_thresholds`, `health_trigger`, and `enabled`. A
provided `health_thresholds` replaces the stored set; `health_trigger` is left
alone when absent and cleared by null.
`POST /api/v1/schedules` accepts `policy_id`, `table_id`, `cron_expression`,
`time_zone`, an optional `window` object with `start_minute` and `end_minute`,
and `service_principal_id`; `PATCH` accepts `revision` plus any of
`cron_expression`, `time_zone`, `window`, and `enabled`. For `window`, an absent
field keeps the stored value, an explicit `null` clears it, and an object
replaces it. Changing the window recomputes a future next-due cursor so a skip
under the old window cannot hide newly eligible runs; an overdue cursor is still
evaluated under the new window. A schedule's policy and table cannot change.

A schedule reads back with two derived fields. `service_identity_name` is the
display name of the service principal it runs as, so a reader names the identity
without a separate grant. `next_occurrences` lists the next five cron instants
strictly after the response was built, computed in the schedule's time zone by
the same DST rules as admission; the maintenance window still applies, so
admission can skip one of them. The instants come from the timing alone, so a
disabled schedule still lists them: disabling stops admission, not the
arithmetic.

`GET /api/v1/schedules/{id}/occurrences` returns the durable evaluations newest
first. Each item carries `scheduled_for`, `coalesced_until` when downtime folded
several instants into one evaluation, the state (`Pending`, `Admitted`,
`Coalesced`, `Skipped`), the job that was created or already owned the table,
and the stable reason for a coalesced or skipped row: `TableBusy` when a
nonterminal job held the table, or `OutsideWindow`, `PolicyDisabled`,
`ConnectionDisabled`, `MaintenancePaused`, `ServiceIdentityIneligible`, or
`HealthTriggerNotMet` when admission refused; the health reason records a
policy condition that is unmet or unassessed. The page is keyed on
`scheduled_for`, which is unique within a schedule, and an unknown schedule
returns 404 rather than an empty page.

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

## Notification destinations

`GET /api/v1/notifications/destinations` pages through the organization's
destinations in ascending identifier order, `POST` registers one,
`GET .../{id}` reads one, and `PATCH .../{id}` edits name, URL reference,
signing-secret reference, or `enabled` when the request names the revision it
read. All four require organization-wide `notifications:manage`, which only the
Administrator role carries. A catalog connection or table grant never covers
them. A repeated `name` answers 409 `resource_already_exists`.

A destination is `webhook` or `slack`. A webhook posts the event envelope and is
always signed; a Slack destination posts one `text` field and is never signed.
Neither the URL nor the key is stored: `url_ref` and `signing_secret_ref` are
bare file names under `notifications.secrets_dir`. The URL file's contents are
read when the destination is created or edited and before every send; the
resolved URL must be HTTPS, carry no user information or fragment, and name a
host from `notifications.allowed_hosts` (configuration, not this API). A Slack
webhook URL embeds a bearer token in its path, so the file, not the database,
holds it. Responses echo the references, never the values, and no audit record
contains them.

`POST .../{id}/test` sends one event with kind `notification.test` and answers
with the destination's HTTP status: a non-2xx status is a completed test with
`delivered: false`, while a 400 `destination_not_allowed` or
`destination_url_unavailable`, a 500 `signing_secret_unavailable` or
`notifications_secrets_dir_unconfigured`, or a 502
`notification_delivery_failed` means the request was not made or could not
complete. Every attempt is audited, including a failed one:
`notification.destination_tested` records the outcome and the stable failure
reason, because a request whose response was lost may still have reached the
receiver. The test does not enqueue a durable delivery and leaves no
delivery-log row.

## Notification rules

`GET /api/v1/notifications/rules` pages through the organization's rules in
ascending identifier order, `POST` registers one, `GET .../{id}` reads one, and
`PATCH .../{id}` edits it when the request names the revision it read. All four
require organization-wide `notifications:manage`. A repeated name answers 409
`resource_already_exists`.

A rule names an `event_kind` (`job.failed`, `job.replan_required`,
`publication.commit_outcome_unknown`, `schedule.occurrence_missed`,
`table.health_critical`, or `workers.pool_without_workers`), one
`destination_id`, and at most one filter: `{"connection_id": "<uuid>"}`,
`{"pool": "<name>"}`, or `{"table_id": "<uuid>"}`. The destination and the
table or connection filter must belong to the calling organization; a missing
one answers 404. The event kind cannot change after creation. A
`repeat_threshold` is accepted only for `job.replan_required`,
`publication.commit_outcome_unknown`, and `schedule.occurrence_missed`, and is
1 or more; absent means every matching event. A threshold counts distinct
source events per rule and table (or resource); once the count reaches it,
later matching events also notify until the counter has been quiet for 24
hours. A replayed source event does not advance the counter. On `PATCH`, an absent `filter` or
`repeat_threshold` keeps the stored value, `null` clears it, and an object or
number replaces it.

## Notification deliveries

`GET /api/v1/notifications/deliveries` pages through the organization's
delivery log, newest first, with `limit` and `cursor`. `state` filters on
`pending`, `delivered`, or `exhausted`; `rule_id` and `destination_id` narrow
the log to one rule or destination. `GET .../{id}` reads one delivery with its
attempt history. Both require organization-wide `notifications:manage`.

A delivery shows the event kind, summary, resource identifiers, and the stable
reason; the attempt history shows the attempt number, completion instant,
outcome (`delivered`, `rejected`, or `failed`), HTTP status, and stable reason.
A response body is never stored, because a destination can echo a credential
from its URL path in an error page. Delivered and exhausted rows are pruned
after 30 days; a pending row remains until it succeeds or reaches the attempt
cap.

## Pagination, errors, and authentication

Lists accept `limit` (default 50, maximum 100) and an opaque `cursor`. Follow
`next_cursor` until it is null. The cursor binds the resource, principal,
organization, and query shape: a cursor from the plain table list is refused by
a health-filtered one and the reverse, and a health cursor repeats the filter it
was produced under, so a page is never resumed under a different filter or
order. Visibility is checked on every page; revocation immediately
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
