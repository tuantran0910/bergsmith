# 0012. Worker service identity: TLS with pool-scoped bearer tokens

- Status: Accepted; token storage superseded by 0055
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §5 Security and deployment

## Context

Workers must authenticate to the coordinator, and every worker session and
attempt must be bound to an authenticated principal. Workers run in pools
with different capabilities and trust. The choice is between mutual TLS and
bearer tokens over server-authenticated TLS.

mTLS needs a certificate authority, issuance, rotation, and revocation for
every worker, which many self-hosted deployments do not have. Bearer tokens
are simple to issue and rotate per pool, but must never leak.

## Decision

- Every `bergsmith.cluster.v1` call uses TLS that authenticates the
  coordinator, and carries `authorization: Bearer <token>` metadata.
- Tokens are scoped to one worker pool and read `bsw1_<pool>_<secret>`, with
  a 32-byte random secret in base64url. The version prefix allows a future
  format.
- The coordinator stores only a verifier,
  `SHA-256("bergsmith.service-token.v1" || 0x00 || token)`, maps it to a
  service principal for the pool, and compares verifiers in constant time.
- `RegisterWorker` must name the token's pool; the session is bound to the
  token's principal, and every later call must present a token for the same
  principal.
- Tokens are redacted from `Debug`, have no `Display`, and are never logged
  or persisted in plain form.
- mTLS remains possible later as an alternative authenticator that maps a
  certificate to the same service principal.

## Consequences

- Deployments only need a coordinator TLS certificate and one secret per pool.
- A leaked token grants a pool's worker rights until rotated. Rotation is
  issuing a new token and removing the old verifier; BS-017 adds the
  management commands and audit.
- `bergsmith-protocol` provides token parsing, metadata extraction, and
  verification; the coordinator enforces it in BS-013.
