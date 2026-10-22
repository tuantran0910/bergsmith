# 0041. Coordinator serve runtime and release packaging

- Status: Accepted
- Date: 2026-10-22
- Plan reference: BS-020.T1; ADR 0037 (web UI), ADR 0039 (schedule admission)

## Context

The repository had every coordinator behavior as a library — identity routes,
the browser API, worker RPCs, scheduling, planning, publication, and worker
supervision — but no process called them. The coordinator binary only migrated
the database and provisioned an administrator, so the UI could not be served,
manual jobs never left `Planning`, and the M5 release gate could not run.
ADR 0037 deferred the one-origin runtime to BS-020.T1; ADR 0039 says the same
runtime must plan scheduled jobs as it plans manual ones.

The runtime also needs operator configuration that did not exist: catalog
storage profiles, planner and scheduler limits, worker token verifiers, OIDC
settings, and the task envelope offered to workers. T1 must make a first
usable release without weakening the architecture invariants.

## Decision

### One process, one active leader

`bergsmith-coordinator serve --config <file>` starts the complete runtime.
It connects to PostgreSQL, acquires the leadership lease, and only then starts
listeners. A process that cannot acquire the lease waits; it does not serve a
read-only or stale API. Leadership is renewed from database time. Losing it
triggers a clean shutdown; the next process acquires a new epoch.

The coordinator never migrates at startup. `migrate` stays a separate command
and the packaging runs it as its own job before any replica starts.

### Configuration

One JSON document holds non-secret settings. Secrets are named by file:
`database.url_file`, `identity.client_secret_file`, and the worker token file
read by the worker. `BERGSMITH_DATABASE_URL`, `BERGSMITH_HTTP_BIND`, and
`BERGSMITH_WORKER_BIND` override the corresponding fields.

The document includes:

- the browser and worker listener addresses;
- the OIDC issuer, client, redirect URI, and scopes;
- the session cookie policy;
- storage profiles by catalog connection, with one default set;
- the base planner configuration and scheduler admission/retry limits;
- the one task envelope and worker pool for this release;
- worker token verifiers, never the tokens themselves;
- leadership and loop timings.

Validation rejects an insecure issuer unless `allow_insecure_http` is set, a
confidential client without a secret file, missing TLS material, duplicate
worker verifiers, and inconsistent planner/scheduler bounds. The configuration
is logged only as sanitized fields; URLs with credentials are never printed.

### Planning and dispatch

A bounded loop reads `Planning` jobs. It resolves one catalog alias the job's
requester may use, loads current metadata and table-scoped storage credentials,
and plans with the base configuration. A scheduled job is planned with its
admitting policy through `PlannerConfig::for_policy`, with the scheduler's
global input cap as the third minimum; a manual job keeps the base limits.

The immutable plan is recorded by the existing `record_outcome` transaction.
Task artifacts are written under
`<data-location>/bergsmith/<job_id>/artifacts/<task_id>.pb`, outside any
attempt output prefix. Their digests and the configured task envelope are
registered with `Scheduler::register`. A crash between the plan and
registration is repaired by the same loop scanning `Queued` jobs without a
dispatch registration. Transient catalog, storage, or database failures leave
the job `Planning` and retry with capped backoff; an authorization or
configuration failure ends it with a stable reason.

### Publication

A bounded loop reads `ReadyToCommit` jobs. It rechecks the planning baseline,
builds the catalog `Baseline` from the current metadata, prepares the durable
intent, and sends the first commit. A baseline change ends the job
`ReplanRequired` without an intent. A live publisher records the response; a
crash or ambiguous transport result leaves the intent and any authorization
durable, and the scheduler's evidence-based recovery handles it. The loop never
resends.

### Browser surface

`serve` always provides `/auth`, `/api/v1`, and `/healthz`. It serves a Vite
build only when `http.web_root` is configured; then static serving rejects
`/api` and `/auth` fallbacks instead of returning HTML to a typed client, serves
hashed assets as immutable, serves `index.html` for client routes, and never
allows path traversal.

The release packaging keeps the UI in a separate `web` image. For the first
release the development Compose stack runs nginx as the browser origin: it
serves the build, proxies `/api`, `/auth`, and `/healthz` to the coordinator,
and keeps long-lived event streams unbuffered. This keeps the Rust image free
of static assets while preserving one origin. A deployment without nginx can
mount a build and set `http.web_root` instead.

Every response from `serve` carries a restrictive CSP, `nosniff`,
`no-referrer`, and a frame denial. `/healthz` is a liveness probe; it does not
claim database readiness.

### Packaging and bootstrap

The multi-stage build has `coordinator`, `worker`, and `web` targets. The
coordinator image contains the Rust binary only; the worker image contains the
worker binary; the web image contains nginx and the production Vite build. Base
images are pinned by digest; the application images are built from this
checkout and can be replaced by digest-pinned release images.

`deploy/docker/compose.app.yaml` is a development stack with PostgreSQL,
RustFS, Lakekeeper, Keycloak, a certificate init job, a migration job, a
bootstrap job, one coordinator, one web origin, and an optional worker profile.
Every credential is fake and the database runs with `fsync=off`; it is not a
production reference. The
bootstrap job seeds a development organization, administrator, worker service
principal, and catalog connection. New tables are registered explicitly with
`register-table` until an inventory API exists.

Administration commands create organizations, catalog connections, table
aliases, and service principals, and print a worker token verifier. They keep
startup wiring in the binary and audited behavior in the library.

## Consequences

- M4's browser acceptance can run against a real coordinator origin, and M5
  manual and scheduled runs share the same planning, admission, execution, and
  publication path.
- One coordinator is active at a time. Standby processes wait for the lease
  instead of serving stale reads; high availability is later work.
- The planning and publication loops run in the coordinator process. They are
  bounded, but a very large catalog read can delay the next scheduler tick; the
  existing per-job timeouts and idempotent retries keep this safe.
- The development Compose stack is not a production certification. Production
  gates (Linux resource enforcement, disk exhaustion, slow storage, real
  catalog/storage combinations, PostgreSQL TLS) remain BS-020.T4/T6.
- A policy's budget is fixed in the immutable plan; tightening it later affects
  only future plans.
