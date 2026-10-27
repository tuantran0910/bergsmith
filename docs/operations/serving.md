# Serving Bergsmith

BS-020.T1 adds the coordinator process that runs the browser API, the worker
RPC endpoint, scheduling, planning, and publication. The UI is a separate nginx
image; `serve` can also serve a build when `http.web_root` is configured. The
decision is [ADR 0041](../adr/0041-coordinator-serve-runtime.md).

## Start the coordinator

```sh
bergsmith-coordinator migrate --database-url "$BERGSMITH_DATABASE_URL"
bergsmith-coordinator serve --config /etc/bergsmith/coordinator.json
```

Migrations are a separate command and must finish before any coordinator
starts. `serve` never changes the schema. A process that cannot acquire the
leadership lease waits; a second coordinator does not serve stale reads.

`BERGSMITH_DATABASE_URL`, `BERGSMITH_HTTP_BIND`, and `BERGSMITH_WORKER_BIND`
override the corresponding configuration fields. Secrets are files, never
configuration values: `database.url_file` and
`identity.client_secret_file`. Worker tokens are read by the worker, not the
coordinator.

## Database TLS

Every command that connects to PostgreSQL (`serve`, `migrate`, and the
administration commands) reads TLS settings from the libpq-compatible URL
parameters or the matching `PGSSL*` environment variables
([ADR 0042](../adr/0042-postgresql-tls.md)). Workers never connect to
PostgreSQL.

| Parameter | Environment | Meaning |
|---|---|---|
| `sslmode` | `PGSSLMODE` | `disable`, `allow`, `prefer` (default), `require`, `verify-ca`, or `verify-full` |
| `sslrootcert` | `PGSSLROOTCERT` | PEM CA bundle that signs the server certificate; without it, the bundled WebPKI roots |
| `sslcert` | `PGSSLCERT` | PEM client certificate, when the server requires one |
| `sslkey` | `PGSSLKEY` | PEM private key for `sslcert` |

Production deployments must use `sslmode=verify-full` with `sslrootcert`, so
the coordinator authenticates the server by certificate and host name:

```text
postgres://bergsmith@db.example:5432/bergsmith?sslmode=verify-full&sslrootcert=/etc/bergsmith/postgres-ca.pem
```

Keep the URL in `database.url_file`, since it also holds the password. Any
weaker mode logs a warning at startup. `require` encrypts but accepts any
certificate, and `prefer` falls back to plaintext. A server certificate that
does not chain to `sslrootcert`, or does not name the host, fails the
connection.

The test Compose PostgreSQL serves TLS with a development CA that
`make services-up` writes to `deploy/docker/.pki/postgres/`, and still accepts
plaintext. Delete that directory and restart the services to rotate it.

## Configuration

The JSON document is validated before any listener starts. A minimal document:

```json
{
  "database": { "url": "postgres://bergsmith@postgres:5432/bergsmith" },
  "http": { "bind": "0.0.0.0:8080", "secure_cookies": true },
  "worker": {
    "bind": "0.0.0.0:7443",
    "certificate": "/etc/bergsmith/tls/cert.pem",
    "private_key": "/etc/bergsmith/tls/key.pem"
  },
  "identity": {
    "issuer": "https://idp.example/realms/bergsmith",
    "client_id": "bergsmith",
    "client_secret_file": "/run/secrets/oidc-client-secret",
    "redirect_uri": "https://bergsmith.example/auth/callback",
    "token_endpoint_auth": "client_secret_basic"
  },
  "storage": { "default": [{ "kind": "s3", "region": "us-east-1" }] },
  "task": {
    "pool": "native",
    "capability": "native-v1",
    "memory_bytes": 1073741824,
    "cpu_millis": 1000,
    "max_duration_seconds": 180,
    "max_output_bytes": 4294967296
  }
}
```

| Section | Meaning |
|---|---|
| `database` | URL or URL file (TLS parameters included), pool size, acquire timeout |
| `http` | Browser bind, optional built UI directory, `Secure` cookies |
| `worker` | gRPC bind and PEM certificate/key presented to workers |
| `identity` | OIDC issuer, client, secret file, redirect URI, scopes, auth method |
| `sessions` | Cookie name, absolute lifetime, idle timeout |
| `storage` | Profiles by backend, with per-connection overrides keyed by connection UUID |
| `planner` | Base target size, thresholds, manifest and duration bounds |
| `scheduler` | Admission budgets, retry bounds, page size, recovery grace |
| `publication` | Deadline for one bounded proposal step |
| `task` | The single pool, capability, and resource envelope for this release |
| `leadership` | Lease and renewal durations; renewal must be shorter than the lease |
| `loops` | Planning and publication scan intervals |
| `statistics` | Metadata-only sampling: interval, retention, page and connection bounds, and the sample's manifest-entry and histogram limits ([ADR 0057](../adr/0057-metadata-only-table-statistics.md)) |
| `observability` | Optional metrics listener and ledger snapshot bounds ([observability](observability.md)) |
| `notifications` | Outbound destinations: allowed hosts, signing-secret directory, request timeout ([ADR 0060](../adr/0060-notification-destinations-and-durable-delivery.md)) |

`notifications.allowed_hosts` is the complete list of hosts a destination URL
may name; an empty list allows none, and a literal link-local address is
rejected even when listed. `notifications.secrets_dir` holds the destination URL
files and webhook signing keys: a destination stores only file names, the URL
file's contents are the endpoint (checked when the destination is written and
before every send), and the signing file's bytes (with one trailing newline
removed) are the HMAC key. Both are read at startup for the allow-list and when
needed for the files, so a key or URL rotation needs no restart while a host
change does. The coordinator sends with no redirects and a
bounded timeout; deployments should still restrict egress at the network layer,
as they do for catalog connections.

The OIDC issuer is used for discovery from the coordinator and must be the
same string the browser receives in the ID token. In a container, make that
hostname resolve both from the browser and from the coordinator (the
development Compose stack uses `idp.localtest.me` and a network alias).

`web_root` is optional. When set, it must contain `index.html`; the process
refuses to start otherwise. When omitted, only `/auth`, `/api/v1`, and
`/healthz` are served, and a reverse proxy owns the UI. The worker certificate
is required. Use a private CA or a certificate whose
SAN covers the worker's `endpoint` hostname; the worker trusts
`ca_certificate`.

## One origin

In the Docker stack, nginx is the browser origin: it serves the Vite build and
proxies `/api`, `/auth`, and `/healthz` to the coordinator. A coordinator with
`http.web_root` set can be the origin directly. In both cases the UI, `/auth`,
`/api/v1`, and `/healthz` share one origin. Static serving (when enabled):

- serves hashed assets as immutable;
- serves `index.html` for client routes;
- never returns HTML for an unknown `/api` or `/auth` path;
- rejects parent-directory path traversal.

Every response carries a restrictive `Content-Security-Policy`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
`X-Frame-Options: DENY`, and a restrictive permissions policy. Put TLS in
front of the listener (a reverse proxy or load balancer) and set
`http.secure_cookies` to `true`.

`/healthz` answers `200` while the HTTP listener is up, for liveness.
`/readyz` answers `200` only when PostgreSQL responds, for readiness. Metrics,
log formats, and worker health are in [observability](observability.md).

## Runtime order

1. Connect to PostgreSQL.
2. Acquire the leadership lease and epoch.
3. Discover the OIDC provider (with bounded retries).
4. Build the scheduler, planning loop, and publication loop.
5. Start the event listener, HTTP server, and worker RPC server.
6. Reconcile publications, active jobs, expired attempts, and due schedules.
7. Plan `Planning` jobs and register their dispatch.
8. Publish `ReadyToCommit` jobs.

A transient catalog, storage, or database failure leaves a job in place and
retries with capped backoff. A baseline change before publication ends the job
`ReplanRequired`; an ambiguous send is never retried by the loop. See
[scheduling](scheduling.md) and [publication](publication.md).

## Compose development stack

```sh
make images
make app-up
```

The UI is at `http://127.0.0.1:8080`, served by the nginx `web` container. The
stack includes PostgreSQL, RustFS, Lakekeeper, Keycloak, a certificate init job,
a migration job, a bootstrap job, one coordinator, and one web origin. The
worker is an opt-in profile because Linux worker execution needs delegated
cgroup v2; start it with `make app-worker` on a suitable host. Sign in as
`alice` with the password in `deploy/docker/keycloak/realm.json`.

This stack is development-only: fake credentials, `fsync=off`, plain-HTTP OIDC,
and non-`Secure` cookies. It is not a production reference and is not a
certification. `make app-down` stops it and discards its volumes.

The catalog initialization job bootstraps Lakekeeper and creates the
`development` warehouse with RustFS credential vending. Replaying initialization
keeps the existing warehouse. The database bootstrap job then creates the
development organization, administrator, worker service principal, and a catalog
connection that names that warehouse. Neither job creates tables.
`make app-bootstrap-check` verifies initialization replay and the worker state
volume's private `0700` permissions without building the application images.
Register a table after creating it in Lakekeeper:

```sh
bergsmith-coordinator register-table \
  --organization development \
  --connection 01900000-0000-7000-8000-000000000004 \
  --namespace db --table events \
  --database-url "$BERGSMITH_DATABASE_URL"
```

Run that command in the Compose network (for example with
`docker compose run --rm coordinator register-table …`) so the catalog
hostname resolves.

Planning outcomes and dispatch registration are fenced by the active coordinator
epoch. A delayed planning failure applies only to the job revision that was
examined; it cannot fail a newer plan after a leadership handoff. Shutdown
cancels an active planning pass. An unexpected background-task exit, including
a panic, makes the coordinator exit unsuccessfully so a supervisor can restart it.

Storage retention checks use at most 16 concurrent requests within the configured
scheduler validation deadline. A healthy source file requires one metadata
request for its size check. Increase `scheduler.validation_timeout_millis` for
catalogs whose supported table sizes and storage latency require a longer check.

## Administration commands

| Command | Purpose |
|---|---|
| `migrate` | Apply pending migrations and exit |
| `create-organization` | Create an organization |
| `add-catalog-connection` | Register a catalog URI and optional warehouse |
| `register-table` | Load a table and record its canonical alias |
| `bootstrap-admin` | Provision the first administrator for an organization |
| `bootstrap-service` | Create a worker or schedule service principal; optionally grant `MaintenanceService` on a table |
| `issue-worker-token` | Issue a pool token for a service identity into a new mode-0600 file |

Upgrading from a configuration with `worker_verifiers`: automatic import is
unsupported. The verifier derivation and the organization/principal/pool
metadata are unchanged, so a migration tool could import the rows, but none is
provided. The supported procedure is to remove the block, create a service
identity per worker pool, and issue replacement tokens through the API or
`issue-worker-token` before upgrading the coordinators. Workers whose old
tokens are not replaced fail closed with `WorkerTokenInvalid`; no
unauthenticated work is accepted.

Identifiers are logged as structured `tracing` fields. The commands are
idempotent where the underlying ledger is: re-running `migrate` or
`bootstrap-admin` is safe, while duplicate organization slugs, connection
names, and principals are rejected.
Service provisioning, its optional grant, and their audit records commit in one
transaction; failure leaves none of those changes behind. `issue-worker-token`
requires an administrator holding `workers:manage`, stores only the verifier,
and writes the bearer token once to a new mode-0600 file. An existing file is
never overwritten; do not pass the token through shell history or a log.
