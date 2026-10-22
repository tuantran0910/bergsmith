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
  },
  "worker_verifiers": [{
    "organization": "…",
    "principal": "…",
    "pool": "native",
    "verifier": "…"
  }]
}
```

| Section | Meaning |
|---|---|
| `database` | URL or URL file, pool size, acquire timeout |
| `http` | Browser bind, optional built UI directory, `Secure` cookies |
| `worker` | gRPC bind and PEM certificate/key presented to workers |
| `identity` | OIDC issuer, client, secret file, redirect URI, scopes, auth method |
| `sessions` | Cookie name, absolute lifetime, idle timeout |
| `storage` | Profiles by backend, with per-connection overrides keyed by connection UUID |
| `planner` | Base target size, thresholds, manifest and duration bounds |
| `scheduler` | Admission budgets, retry bounds, page size, recovery grace |
| `publication` | Deadline for one bounded proposal step |
| `task` | The single pool, capability, and resource envelope for this release |
| `worker_verifiers` | Organization, service principal, pool, and token verifier |
| `leadership` | Lease and renewal durations; renewal must be shorter than the lease |
| `loops` | Planning and publication scan intervals |

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

`/healthz` answers `200` while the HTTP listener is up. It does not check the
database, the catalog, or leadership; deeper diagnostics are BS-020.T2.

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

The bootstrap job creates the development organization, administrator, worker
service principal, and one catalog connection. It does not invent tables.
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

## Administration commands

| Command | Purpose |
|---|---|
| `migrate` | Apply pending migrations and exit |
| `create-organization` | Create an organization |
| `add-catalog-connection` | Register a catalog URI and optional warehouse |
| `register-table` | Load a table and record its canonical alias |
| `bootstrap-admin` | Provision the first administrator for an organization |
| `bootstrap-service` | Create a worker or schedule service principal; optionally grant `MaintenanceService` on a table |
| `service-token-verifier` | Print the verifier to put in `worker_verifiers` |

Identifiers are logged as structured `tracing` fields. The commands are
idempotent where the underlying ledger is: re-running `migrate` or
`bootstrap-admin` is safe, while duplicate organization slugs, connection
names, and principals are rejected.
