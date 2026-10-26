# 0060. Notification destinations and durable delivery

- Status: Proposed
- Date: 2026-10-27
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-029

## Context

An operator learns that a job failed, that a table turned critical, or that a
pool has no live workers only by watching the UI or scraping metrics. BS-029
delivers those events to systems the operator already runs: an HTTP endpoint
that verifies an HMAC signature, or a Slack-compatible webhook.

Three constraints shape the decision.

**Events are ledger facts.** A failed job, a `ReplanRequired` transition, and a
resolved `CommitOutcomeUnknown` are written inside the transaction that changes
the job state (ADR 0004, ADR 0036). A notification must not be lost because the
coordinator crashed between committing that transaction and publishing an
event, and it must not be announced for a transaction that rolled back.

**Secrets stay outside PostgreSQL.** Bergsmith stores credential *references*
and resolves them from configuration (plan §5, "Security and deployment";
ADR 0005). A webhook signing secret is the same kind of value: the database may
hold its name, never the key.

**An outbound request is an operator-authorized action.** The coordinator makes
network calls to catalogs and to the OIDC provider; nothing else. A destination
URL that an administrator can type is a request-forgery surface: without a
bound, it can reach the metadata service, loopback services, or arbitrary
hosts.

## Decision

**Destinations are organization-scoped records with an optimistic revision.**
`notification_destinations` holds a name, a kind, a URL, an optional signing
secret reference, `enabled`, `revision`, and the creating principal. Two kinds
exist: `webhook` posts a Bergsmith event envelope as JSON and is always signed;
`slack` posts a Slack-compatible `{"text": ...}` body and carries no signature.
Disabling is an update that flips `enabled`, like policies and schedules.

**The URL must be HTTPS and its host must match a configured allow-list.**
`notifications.allowed_hosts` is deployment configuration, not a database
column. Each entry is a host with an optional port, or `*.suffix` for
subdomains at the same port. Matching is case-insensitive and exact on the port
(absent means the scheme default). A literal link-local address (`169.254.0.0/16`,
`fe80::/10`, and their IPv4-mapped forms) is rejected even when an entry names
it, because the cloud metadata service lives there. Loopback and private
addresses are allowed only when an entry names that exact host, which is how
development and test servers are reached. The check runs when a destination is
created or edited and again before every send, so tightening the configuration
takes effect for existing rows. DNS rebinding under an allow-listed name is out
of scope; deployments restrict egress at the network layer, as they already do
for catalog connections.

**Signing secrets are references.** A webhook destination stores
`signing_secret_ref`, a name matching `[A-Za-z0-9._-]{1,128}` that is neither
`.` nor `..`. The coordinator reads `notifications.secrets_dir/<ref>` when it
sends; the value never enters the database, an API response, a log, or an
error. A missing or unreadable secret fails that delivery permanently with
`signing_secret_unavailable`; an unsigned request is never sent.

**The signature is Stripe-compatible.** The request carries
`X-Bergsmith-Signature: t=<unix>,v1=<hex>`, where the MAC is
HMAC-SHA256(secret, `<t>.<body>`) with lowercase hex. A receiver recomputes it
and rejects a timestamp it considers too old. The header name, the two fields,
and the signed string are the public contract.

**Payloads carry identifiers and reasons, never credentials.** The webhook
envelope is `{"id","kind","occurred_at","organization_id","summary","resource":
{"type","id","label"},"reason"}`. `kind` is a stable event name, `summary` is
one sentence for a human, `resource` names the entity the event is about, and
`reason` is the stable code the ledger recorded. A Slack body is
`{"text": "<summary> — <resource label>"}`. No plan artifact, catalog
credential, token, or table row is included. The outbound client has a
10-second timeout, sends no redirects, and reads at most 1 KiB of the response
for the delivery log.

**Delivery is a PostgreSQL outbox, at least once.** A rule match appends a
`notification_deliveries` row in the same transaction as the event that
triggered it. A leader-only loop claims due rows, sends one request, and
appends a `notification_attempts` row per try. Retries back off 30 s, 1 min,
5 min, 15 min, and 1 h, and stop after five attempts; each step is recorded.
The loop claims with `SELECT ... FOR UPDATE SKIP LOCKED` in one transaction,
sends outside any transaction, then marks the row in a second transaction, so
no database transaction spans the request. A crash after a successful send and
before the mark produces one duplicate; that is the documented cost of
at-least-once delivery, and receivers deduplicate on the envelope `id`.
Delivered and exhausted rows are pruned after 30 days.

**Rules are organization-scoped and narrow one destination.** A rule names an
event kind, a destination, an optional connection, pool, or table filter, and
an optional repeat threshold; `ReplanRequired` and missed occurrences are the
kinds that need one. Rules have an optimistic revision and an `enabled` flag.

**Administration is `notifications:manage`, held by Administrator.** Creating,
editing, testing, and disabling destinations and rules, and reading the
delivery log, require that permission. Sending a test request also requires it.

## Consequences

- An operator can wire a signed endpoint or a Slack channel without giving
  Bergsmith a new credential store; the signing key stays in a mount.
- The allow-list is deployment policy; adding a destination host is a
  configuration change and a restart, not a UI action.
- Events that have no write point today (a table turning critical, a pool with
  no live workers) need a producer in the loop that observes them: the
  statistics collector records the health transition, and the delivery loop
  samples pool liveness. Their rules therefore fire on the coordinator's
  observation, not on a database transaction.
- At-least-once means receivers must tolerate a duplicate after a crash;
  envelope IDs make that possible.
- The outbox adds two tables and one background loop, and the loop is the
  first `SKIP LOCKED` claim in the codebase.
