# Identity and authorization

People sign in through OpenID Connect. Every command is checked against the
actor's active role grants, at the API boundary and again when the coordinator
admits a job or authorizes a publication send. Workers are unaffected: they use
pool-scoped bearer tokens over TLS (see [workers](workers.md)).

The architecture decision is [ADR 0032](../adr/0032-oidc-sessions-and-scoped-authorization.md).

## Roles and permissions

| Permission | Viewer | Operator | Administrator | MaintenanceService |
|---|---|---|---|---|
| `inventory:read` | yes | yes | yes | yes |
| `jobs:read` | yes | yes | yes | yes |
| `artifacts:read` | yes | yes | yes | yes |
| `jobs:submit` | — | yes | yes | yes |
| `jobs:retry` | — | yes | yes | yes |
| `jobs:cancel` | — | yes | yes | — |
| `connections:manage` | — | — | yes | — |
| `schedules:manage` | — | — | yes | — |
| `workers:manage` | — | — | yes | — |
| `grants:manage` | — | — | yes | — |
| `audit:read` | — | — | yes | — |

A grant is scoped to the whole organization, one catalog connection, or one
table. An organization grant sees everything in the organization; a connection
grant sees that connection's tables and not another connection's; a table grant
sees only its table and not the connection. Inventory and search always apply
this filter, so a restricted principal cannot scan another connection's tables.

## Sign-in

1. The application starts `GET /auth/login?organization=<slug>&return_to=/path`.
2. The coordinator creates a single-use login state (ten-minute window), binds
   it to the browser with a short-lived `HttpOnly` cookie, and redirects to the
   provider with `state`, `nonce`, and an S256 `code_challenge`.
3. `GET /auth/callback` requires the binding cookie, consumes the state and
   deletes it, exchanges the code with the stored PKCE verifier, verifies the
   ID token, and issues a session. A captured state cannot be redeemed in
   another browser.
4. The browser receives an `HttpOnly` session cookie and a readable CSRF
   cookie. Unsafe requests must echo the CSRF value in `x-csrf-token`.
5. `POST /auth/logout` revokes the session. `GET /auth/session` returns the
   current principal and grants.

A principal must already exist in the organization with the token's issuer and
subject. The coordinator never creates a principal or a grant during login.

## Configuration

The coordinator `serve` runtime reads these from its JSON configuration
([serving](serving.md)); the OIDC issuer must be reachable from the coordinator
and be the same string the browser receives in the ID token.

| Setting | Meaning |
|---|---|
| Issuer URL | Compared exactly with the `iss` claim; discovery lives under it |
| Client ID and secret | The confidential client registered with the provider |
| Redirect URI | Must match the provider's registration |
| Scopes | `openid` is always requested |
| Token endpoint auth | `client_secret_basic` by default |
| `allow_insecure_http` | Development only; required for the Compose fixture |

Rejections use stable reason codes (`OidcSignatureInvalid`, `SessionRejected`,
`AuthorizationDenied`, and so on) and never include a token, claim, or client
secret.

## Development provider

`deploy/docker/compose.yaml` runs Keycloak `26.0.7` with
`deploy/docker/keycloak/realm.json`, which defines the `bergsmith` realm, the
confidential `bergsmith` client, and the user `alice`. The issuer is
`http://127.0.0.1:58080/realms/bergsmith`. All values are obviously fake and
the service uses an in-memory database; it is never used in production or by
the automated tests.

## Operator duties

- Provision principals and grants before onboarding a user; login does not
  provision.
- Bootstrap the first administrator with
  `bergsmith-coordinator bootstrap-admin --organization <slug> --issuer <url>
  --subject <sub>`. The command is idempotent and audited as a system action.
- Revoke a grant to stop future admission and publication for that principal.
  Revoking a grant does not cancel a running job; cancel it explicitly.
- Jobs that predate scoped authorization are attributed to a per-organization
  `bergsmith:system` principal with an organization-wide `MaintenanceService`
  grant, so they finish rather than being stranded. Revoke that grant to halt
  legacy work at its next admission or publication check.
- Read the audit log to attribute denied actions, sign-ins, and grant changes.
- Rotate the OIDC client secret and worker pool tokens independently; the two
  credential systems do not share storage.

## Limits

- One organization may have at most 32 outstanding login attempts; excess
  requests get `429 rate_limited`. Expired login states and sessions are
  reclaimed in bounded batches, and the callback never holds a database
  connection across the provider call.
- A session refreshes its idle timestamp at most every 30 seconds and at least
  every half idle timeout, so short idle timeouts still slide correctly.
- A failed audit record fails a privileged command (grants, provisioning,
  session revocation) so a change is never applied unrecorded; job decisions
  log a failed audit and continue because the decision is already durable.

## Restrictions

- HTTPS is required except when the development fixture enables plain HTTP.
- Only `RS256`, `RS384`, and `RS512` ID tokens are accepted.
- Return paths are relative; absolute and scheme-relative URLs are rejected.
- Audit detail objects may contain identifiers and stable reason codes only.
