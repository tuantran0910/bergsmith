# 0007. Catalog-vended credentials and attempt output location

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §5 Security and deployment; §3 Input retention and output lifecycle

## Context

In the target deployments, the catalog is the only path to storage. Vended credentials are short-lived and scoped to the table location.

## Decision

- The coordinator loads tables with `X-Iceberg-Access-Delegation: vended-credentials`. Grants carry the table-scoped credentials and their expiry.
- The coordinator refreshes them via the `/credentials` endpoint (or a table reload) and returns them in heartbeat responses; the supervisor pushes them to the task subprocess.
- Credentials live only in memory: never persisted, logged, or written to artifacts.
- Attempt outputs are written to `<data-location>/bergsmith/<job_id>/<attempt_id>/`, inside the table location.
- Remote signing is not supported.

## Consequences

- External orphan cleaners must exclude the `bergsmith/` prefix or use an `older_than` longer than the maximum unresolved-publication age.
- Credential expiry during a task is a `RetryTask` failure.
- The development store must support the STS flow the catalog vends from (verified in BS-005).
