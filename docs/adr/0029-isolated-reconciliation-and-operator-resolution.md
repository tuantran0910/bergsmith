# 0029. Isolated reconciliation, publication grace, and operator resolution

- Status: Accepted
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2 (job lifecycle,
  failure dispositions, durable state), §3 (commit and crash recovery),
  BS-012.T2/T4, BS-014.T2–T4

Refines [0022](0022-durable-publication.md) and
[0025](0025-durable-admission-and-reconciliation.md); the parts of those
decisions not restated here stand.

## Context

Review of BS-012 and BS-014 found that one job's catalog outage, missing
binding, or slow validation aborted the whole reconciliation pass and closed
dispatch for every tenant, while every admitted job was revalidated each second.
Periodic publication recovery could mark a send still in flight from the live
publisher unknown. Stops used the current revision rather than the one the
decision was based on. A coordinator 401 failed every job on a catalog. A token
failure after send authorization failed a job permanently. Nothing could ever
resolve `CommitOutcomeUnknown`, publication writes were not leadership-fenced,
and worker-reported NaN counts reached manifests unverified.

## Decision

- Reconciliation isolates job-scoped failures. Catalog, storage, binding,
  stored-plan, and concurrent-state errors skip that job with a logged reason
  code; the pass continues. Only lost leadership and database failure fail the
  pass and close dispatch. Refill skips unusable tasks the same way.
- External validation runs only before admission (when the table and budgets
  have room) and before releasing a due retry. An admitted job without due
  retries keeps its admission validation; publication rechecks the baseline.
  Expired-attempt fencing needs no validation and runs every pass.
- Stops carry the revision whose state the decision used. A job that moved
  meanwhile is left unchanged and decided again later.
- The first pass of a scheduler recovers every publication. Later passes skip a
  `Committing` job until `publication_send_grace` (default 120 s, database
  time) has passed since send authorization, leaving it to its live publisher.
- A catalog 403 on validation is revoked authority (permanent). 401/419 and
  token-endpoint refusals concern the coordinator credential: they are
  transient, and the client discards a rejected cached OAuth2 token.
- The commit request, including its OAuth2 token, is authenticated before send
  authorization. Failure there leaves the job unauthorized and publishable.
  After authorization only a connection failure can prevent the send; the live
  sender's proof still ends the job `Failed` (or its pending/cancellation
  disposition), because authorization stays one-shot.
- Publication preparation bounds each storage step separately (per task
  verification, manifest list, each manifest, proposal write) with a
  configurable `PublicationConfig::operation_timeout` (default 60 s).
- `resolve`, `pend_disposition`, publication reconciliation, and operator
  attestation check the leadership epoch inside their transactions.
- Periodic passes keep re-reading evidence for `CommitOutcomeUnknown`; matching
  evidence records `Succeeded`. Absence, elapsed time, or a different head never
  resolve it. An operator who has established outside Bergsmith that no
  authorized request published or can still publish may call
  `durable_publication::attest_non_publication`. It rechecks evidence first
  (matching evidence always wins), requires the inspected revision, and applies
  cancellation, the pending disposition, or `Failed`, with reason
  `OperatorAttestedNonPublication` and the operator as the immutable job-event
  actor. Unknown jobs keep their barrier and admission slots until resolved;
  the scheduler exposes and logs the unresolved count.
- Verified data-file descriptors omit NaN counts. Footers cannot confirm them,
  and absent counts mean "unknown" to readers. This supersedes the NaN range
  check in 0022 as the basis for publishing them.

## Consequences

One tenant's outage or a large slow job no longer stops dispatch elsewhere.
Large unadmitted jobs still pay one bounded validation per admission attempt.
Operators now have an audited, evidence-guarded exit from unknown outcomes; a
wrong attestation can hide a real publication, so the operations guide lists
what must be checked. Publishers must not drive `publish` before the scheduler's
first successful pass. Readers cannot prune on NaN for Bergsmith outputs.
