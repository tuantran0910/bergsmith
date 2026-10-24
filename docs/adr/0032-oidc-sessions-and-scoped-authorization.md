# 0032. OIDC sessions and scoped authorization

- Status: Accepted; refined by [0033](0033-every-job-names-its-requester.md) and [0054](0054-session-permission-projection.md)
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4 BS-017, §5 Security and deployment

## Context

People sign in with OIDC Authorization Code and PKCE; schedules must act under
an explicit service identity; and every maintenance command needs a permission
model that is re-evaluated at admission and before a commit is sent. Worker
identity already exists as pool-scoped bearer tokens over TLS
([0012](0012-worker-service-identity.md)); this decision must not disturb it.
The audit log must attribute decisions without storing credentials.

## Decision

**Sessions.** The coordinator is a confidential OIDC client. It discovers the
provider from `<issuer>/.well-known/openid-configuration`, performs the
authorization-code flow with a 32-byte PKCE S256 verifier, and verifies the ID
token itself. Every provider HTTP call (discovery, token exchange, JWKS) is
bounded by a configured timeout. Only `RS256`, `RS384`, and `RS512` are
accepted; `none`, HMAC, and EC algorithms are refused. Issuer, audience,
authorized party, expiry, not-before, and nonce are validated independently of
signature checking. `iat` is required by OpenID Connect Core, so a token that
omits it (or sends null) is malformed rather than unchecked; a future `iat` is
rejected within the configured skew.

**Tokens.** A browser holds a session token and a CSRF token. PostgreSQL stores
only domain-separated SHA-256 digests (`bergsmith.session-token.v1`,
`bergsmith.csrf-token.v1`) and compares them in constant time, so a database
reader cannot replay a session. A login attempt is bound to the browser that
started it by a second `HttpOnly` cookie whose digest is stored with the login
state; a captured `state` cannot be redeemed in another browser. Consuming a
login state deletes it, so the PKCE verifier does not outlive the exchange.
Outstanding login attempts are capped per organization, and expired
login states and sessions are reclaimed in bounded batches. Sessions are
revoked by logout or administratively; identity binding re-checks the
principal's issuer and subject on every request, and only `kind = User`
principals may sign in interactively.

**Cookies.** The session cookie is `HttpOnly`, `SameSite=Lax`, and `Secure`
outside development. The CSRF cookie is readable so the application can echo it
in `x-csrf-token`; unsafe requests fail without it. Return paths are relative
and validated in Rust and by a schema check that also rejects backslashes and
whitespace, which browsers could normalize into a scheme-relative URL.

**Authorization.** A `Role` is a named set of permissions and a `GrantScope` is
the organization, one catalog connection, or one table. Authorization reads
active grants from the ledger on every decision, so revocation takes effect at
the next check. Admission re-checks inside its transaction, and send
authorization re-checks after the `publication_started_at` guard, before the
one-shot send. A revoked requester ends the job `Failed` with
`AuthorizationRevoked`, but only through a resolution that re-confirms, under
the job lock, that no other sender has been authorized: send authorization
does not bump the job revision, so an earlier denial must never release
authority that a later sender obtained after the grant was restored. Worker
sessions and tokens are untouched. Jobs created
before BS-017 are attributed to a per-organization system principal, and a
requester is mandatory ([0033](0033-every-job-names-its-requester.md)); the API
always records a requester.

**Inventory.** Lists filter by grants: an organization grant sees everything, a
connection grant sees that connection's tables, and a table grant sees only its
table and not the connection. There is no unfiltered scan path.

**Service identity.** A schedule binds one organization service principal
(`kind = Service`) that holds a `MaintenanceService` grant. Revoking that grant
stops admission and publication for its jobs.

**Audit.** `audit_events` is append-only and records actor kind, actor, action,
resource, outcome, and a secret-free detail object. Login success/failure,
logout, session denial, grant changes, session revocation, and job
admission/publication decisions are recorded; `/auth/callback`, `/auth/logout`,
and `/auth/session` audit rejections as well as successes. A denial raised
after the session authenticated keeps the actor and its kind (for example a
CSRF failure); an unauthenticated denial, including a malformed cookie, is
recorded as `Anonymous`. Privileged commands
(a grant, provisioning, session revocation) commit their change and audit row in
one transaction, so a failed audit rolls the change back. Appends are
serialized with a transaction-scoped advisory lock, so a cursor over committed
rows cannot skip a lower identifier that commits late. The schema rejects the
obvious credential keys and a `BEFORE UPDATE OR DELETE` trigger makes rows
immutable.

**Development.** `deploy/docker/compose.yaml` adds a pinned Keycloak with an
imported realm for local sign-in. Tests use a mock provider and real
PostgreSQL; no test depends on the Keycloak container.

## Consequences

- Sessions survive a coordinator restart because they live in PostgreSQL.
- PKCE verifiers live for at most the login window (default ten minutes).
- A principal that authenticates but is not provisioned in the organization is
  refused; provisioning stays an explicit administrative action, which BS-018
  exposes and audits.
- No new workspace dependency beyond `axum` (already resolved through tonic),
  `base64`, `ring`, and the existing `reqwest`, `sha2`, and `url`.
- Commands keep a small, explicit permission surface; expanding concurrency or
  rebasing later does not require touching identity.
