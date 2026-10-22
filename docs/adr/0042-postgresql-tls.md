# 0042. PostgreSQL TLS through libpq connection parameters

- Status: Accepted
- Date: 2026-10-22
- Plan reference: BS-020.T6; ADR 0011 (execution ledger), ADR 0041 (serve runtime)

## Context

BS-003 deferred TLS for the ledger connection. SQLx was built without a TLS
backend, so every coordinator connection to PostgreSQL was plaintext and an
`sslmode` other than `disable` or `prefer` failed. The connection carries the
database password, the ledger, and session and audit records.

Only the coordinator connects to PostgreSQL. Workers never do (architecture
invariant 1), so there is no worker-side database connection to protect.
The coordinator connects from `serve`, from `migrate`, and from the
administration commands, which each take only a URL (`--database-url`,
`BERGSMITH_DATABASE_URL`, or `database.url_file`).

## Decision

- SQLx is built with rustls on the aws-lc-rs provider
  (`tls-rustls-aws-lc-rs`), the same provider the worker channel uses. It
  trusts the bundled WebPKI roots unless `sslrootcert` names a CA.
- TLS is configured only with the libpq-compatible URL parameters (`sslmode`,
  `sslrootcert`, `sslcert`, `sslkey`) or their `PGSSL*` environment variables.
  One setting therefore covers every command, and a URL file keeps the
  certificate paths next to the password. There is no separate TLS section in
  the `serve` document.
- The default stays libpq's `prefer`, so existing development URLs keep
  working. Every command logs a warning when the effective mode is weaker than
  `verify-full`. Production deployments use `sslmode=verify-full` with
  `sslrootcert`.
- The test Compose PostgreSQL serves TLS with a development CA that
  `scripts/dev-pki.sh` generates on `make services-up` (ignored by git), and
  still accepts plaintext. Integration tests prove that `require` encrypts,
  `verify-full` succeeds with that CA, and `verify-full` rejects a server
  signed by an unrelated CA.

## Consequences

- Operators get TLS and client certificates without new configuration
  surface, using parameters their PostgreSQL tooling already understands.
- A weak mode is a warning, not a startup failure. Rejecting it would break
  development stacks; the operator documentation states the production
  requirement instead.
- `make services-up` now needs the `openssl` command, which the distributed
  tests already require.
