# 0045. Operator commands for roles and non-publication attestation

- Status: Accepted
- Date: 2026-10-22
- Plan reference: BS-020.T7; ADR 0029 (reconciliation and attestation), ADR 0032 (scoped authorization)

## Context

The first release could create only the first administrator
(`bootstrap-admin`). Granting anyone else a role, and resolving a
`CommitOutcomeUnknown` job whose snapshot evidence can no longer appear, had
audited library functions but no entry point. The acceptance run needs both.

Attestation writes job state, and every publication-state write is fenced by
the coordinator leadership epoch. Only the `serve` process holds an epoch,
and a second process cannot acquire one while the leader's lease is live.

## Decision

- The coordinator binary gains `grant-role`, `revoke-role`, `list-grants`, and
  `attest-non-publication`. Like the other administration commands they run
  with the database URL; database access is their authentication.
- Each command takes `--actor`, a principal that must hold `grants:manage`
  over the changed scope (for attestation, the job's table; for
  `list-grants`, the organization, because the listing names every principal
  and what it may do).
  The actor is recorded in the job event and audit records, so a principal
  that could not make the change through the API is never recorded as having
  made it.
- `grant-role` provisions a new user principal by issuer and subject before
  granting, as two audited changes. It grants Viewer, Operator, or
  Administrator, and only to user principals: an issuer and subject that
  name a service principal is refused. `MaintenanceService` stays with
  `bootstrap-service`.
- `attest-non-publication` borrows the epoch of the live leadership lease. It
  refuses before any catalog call when no lease is live. The existing fence
  checks the epoch under the job lock inside the write transaction, so a
  leadership change before commit refuses the write, and the inspected job
  revision serializes the command against a concurrent reconciliation. It
  rechecks retained snapshot evidence first; a match records `Succeeded`.
- Attestation requires a reason, kept in a new `job.non_publication_attested`
  audit record. The catalog recheck is network I/O and runs before the write
  transaction; its result is passed in, and the transition, barrier release,
  and audit record commit together. A failed audit write therefore leaves the
  job unresolved, and the operator can run the command again with the same
  revision; no resolution exists without its evidence reference.

## Consequences

- Operators can onboard users and settle unknown outcomes without database
  edits, and the acceptance run can exercise both.
- Attestation needs a running coordinator. During an outage it waits for the
  coordinator to return, which is safe because the table stays blocked.
- Role administration is command-line only; browser administration pages
  remain future work.
