# 0013. Bergsmith owns its catalog HTTP calls

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Pinned integration baseline, §5 Security and deployment

## Context

The coordinator must hand catalog-vended storage credentials to workers in
grants and heartbeats. iceberg-rust's `RestCatalog` loads them, but keeps them
inside a crate-private credential provider attached to `FileIO`; neither the
provider nor the REST `HttpClient` is public at the pinned revision. ADR 0005
already moved commits to Bergsmith's own `CommitTableRequest` client.

## Decision

- `crates/catalog` makes every coordinator catalog call itself with
  `reqwest`: `GET /v1/config`, OAuth2 client credentials or a static bearer
  token, listing tables, loading tables with
  `X-Iceberg-Access-Delegation: vended-credentials`, `GET .../credentials`,
  and (BS-006) commits.
- It reuses iceberg-rust's public REST wire types (`LoadTableResult`,
  `TableMetadata`) instead of redefining them.
- Each request has connect and request timeouts and is never retried
  implicitly. Errors keep the HTTP status and decoded error model and never
  contain credentials.
- A catalog's `uri` override is ignored, so a catalog cannot redirect
  Bergsmith to another host; its `prefix` is honored.
- Connecting fails if the catalog does not advertise listing, loading, and
  committing tables. Credential refresh uses `/credentials` when advertised
  and reloads the table otherwise.
- `reqwest` uses rustls with native roots; feature unification also gives
  iceberg-rust's own `reqwest` TLS.

## Consequences

- No iceberg-rust patch is needed to expose credentials.
- Bergsmith maintains a small HTTP layer and must follow REST specification
  changes itself.
- Lakekeeper v0.14.0 advertises `idempotency-key-lifetime: PT30M`, which
  BS-006 and BS-012 can use for exact commit replays.
- OAuth2 is tested against a mock token endpoint until the OIDC development
  provider arrives (BS-017).
