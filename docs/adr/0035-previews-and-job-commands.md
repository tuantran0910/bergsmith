# 0035. Previews and job commands

- Status: Accepted
- Date: 2026-10-21
- Plan reference: BS-018.T2, public API and UI

## Context

T1 exposed catalog, table, job, and audit reads plus idempotent submission,
and reserved previews for this task. BS-018.T2 must let an authorized operator
assess a table without creating work, cancel a job under the §2 cancellation
contract, and retry a terminal job with a fresh plan. The coordinator has a
planner, a durable cancellation primitive, and job creation, but no runtime that
resolves connection credentials or storage profiles.

## Decision

### Previews are reads with a scoped connection

- `POST /api/v1/previews` accepts `{"table_id": "<canonical UUID>"}` and creates
  no job, plan, task, output, idempotency key, or audit record. It reports what
  a job would be built from; it does not admit work or reserve capacity.
- Resolve the canonical identity from the ledger first, so an unknown table is
  404 while a known table the caller may not submit for is 403 and audited.
  Then authorize `jobs:submit` scoped to the table, and only then contact a
  catalog. The ledger connection is released before any catalog or object-store
  I/O.
- Choose the catalog name among **aliases whose connection the caller's grant
  covers**, in canonical order (connection ID, namespace, name). A
  connection-scoped grant therefore never causes a request through another
  connection, and repeated calls use the same name. This mirrors the alias
  filtering the table read already applies.
- Storage profiles are configured **per catalog connection**, with a fallback
  for connections without an entry. Selecting by backend alone would sign
  requests for the wrong store when two S3-compatible connections have
  different endpoints.
- Verify that the metadata the catalog returned has the canonical
  `table-uuid` immediately after each load, before storage selection or
  credential vending. A drop/recreate or reassigned alias between resolution and
  load is a typed 409 `table_identity_changed`, not a preview of a different
  table and not a storage or credential error caused by the replacement.
- Connection authentication comes from stored settings. A connection that
  names an external credential reference is refused (500
  `catalog_credential_unavailable`) rather than falling back to ambient
  identity; resolving those references is runtime follow-up.
- Catalog clients are reused per connection while the stored URI and warehouse
  are unchanged, so repeated previews do not repeat discovery or a TLS
  handshake. The credential-reference check runs before the cache is consulted,
  so editing a connection is never served from stale settings.
- Interactive previews are bounded: at most 1,000 bins per response (with
  `bins_total` and `bins_truncated`), at most 100,000 manifest entries, at most
  8 concurrent assessments per process, and at most 2 per requester, whose
  excess requests receive 503 `preview_capacity_exhausted` with
  `RetrySameRequest`. These bounds are deliberately stricter than a planning
  job's. CPU-heavy manifest evaluation still runs on the async runtime thread;
  moving it to a blocking task is a follow-up shared with the planning path.
- The wire format is explicit and independent of maintenance types: typed
  rejection names are flattened into one object with nullable variant-specific
  fields; bins expose the partition key, input tally, removal estimate, and a
  stable disposition name. Baseline snapshot and sequence number are exposed;
  metadata and table locations, credentials, and connection settings are not.
  Per-file rejection paths are included because they are what explains a
  refusal.
- Failure classification distinguishes causes a client can act on: unknown
  canonical table 404; catalog table missing 404 `catalog_table_missing`;
  identity change, staged table, over-limit inventory, inconsistent metadata,
  and a catalog-reported invalid table or data location 409 `PermanentFailure`;
  catalog transport failures, timeouts, throttling, undecodable bodies, and
  upstream 5xx 503 `RetrySameRequest`. A catalog that rejects the coordinator's
  credentials (401/403/419, or a 4xx from the token endpoint), lacks required
  endpoints, or cannot vend the table's storage credentials is a permanent 500
  with an operator action, because repeating the request unchanged cannot work.
  Other catalog 4xx responses are permanent 500 `catalog_request_rejected`.
  Configuration gaps, such as an unresolvable credential reference or a missing
  storage profile, are also permanent 500s with an operator action.
- A failed preview is logged with the table, the stable API reason, a sanitized
  error kind, and an upstream HTTP status when there was one. The upstream
  message, URLs, table locations, and Iceberg text are never interpolated: those
  are values the coordinator does not control. A regression test drives a
  catalog error whose body contains a sentinel and proves the log excludes it.

### Cancellation commits with its audit record

- `POST /api/v1/jobs/{id}/cancel` requires `jobs:cancel` on the job's table and
  accepts an optional bounded reason. Authorization, the cancellation, and the
  `job.cancellation_requested` audit record commit in one transaction, so a
  request the audit cannot record leaves the job unchanged. The grant is
  checked inside that transaction.
- The request is idempotent and monotonic by the cancellation contract:
  repeating it returns the same disposition, and the first requester and reason
  are kept. The audit records the reason **actually persisted**, not the one
  that lost a repeat.
- It reports `cancelling`, `too_late_publication_started`,
  `publication_unresolved`, `too_late_committed`, `cancelled`, or
  `already_ended`, with the job's state and revision. A late or already-ended
  request is a truthful 200, not an error.

### Retry is a new job, never a revived one

- `POST /api/v1/jobs/{id}/retry` requires `jobs:retry` on the table and a
  required `Idempotency-Key`. Only a terminal job that did not succeed may be
  retried: a non-terminal source is 409 `job_not_terminal`, and a succeeded
  source is 409 `job_already_succeeded`.
- Authorization is checked before the source's state is reported, so an
  unauthorized caller cannot learn it, and again inside the creation
  transaction, closing the revocation window.
- The retry is a new `Planning` job whose `retry_of_job_id` names the source.
  No plan, output, grant, or accepted result is reused, and a new key creates
  another independent retry.
- Submission and retry share one creation path: authorize, bind the key,
  create the job and its `Created` event, append the audit record, and commit.
  Nothing is committed if any step fails.

### Contract

- Every new route is documented in the checked-in OpenAPI contract, and route
  tests validate real responses against it. Tests also assert that every
  serialized rejection, skip reason, key problem, and bin disposition appears
  in the documented enum, so a Rust rename cannot silently change the contract.
- Preview and command failures carry the same typed `reason`, `disposition`,
  and `operator_action` shape as the rest of the API.

## Consequences

Previews work against the same catalog authority publication uses and never
weaken the "coordinator publishes, workers rewrite" boundary. The API stays
composable: the runtime must still supply per-connection storage profiles and a
credential resolver before a production server can preview authenticated
catalogs, and it must pass the same session policy to both routers.

Cancellation keeps the one-writer send-authorization rule; the command only asks
the ledger, so it cannot race a publication decision. Retry makes
`ReplanRequired`, `Failed`, `Cancelled`, and `NoOp` recoverable without editing
a terminal job.

The preview contract is narrower than the planner: it has no partition-selection
input yet, so it always assesses the whole table, and its inventory bound is
lower than a planning job's. Rejections carry object paths, which are not
credentials but do name storage objects; this matches the operator's need to fix
the offending file. The response is not cached yet; an ETag on the assessed
baseline is a follow-up for the UI.
