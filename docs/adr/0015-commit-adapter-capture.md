# 0015. Build commits with a capture-only catalog

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Pinned integration baseline; §3 Standard catalog commit adapter; §3 Commit and crash recovery
- Supersedes: [0005](0005-commit-path.md)

## Context

ADR 0005 planned to call `ReplaceFilesAction::commit` on the planning
baseline. At the pinned revision that method belongs to the crate-private
`TransactionAction` trait, so code outside iceberg-rust cannot call it. The
only public way to run an action is `Transaction::commit(&dyn Catalog)`,
which reloads the table, re-applies actions on a newer base, and retries
retryable errors.

iceberg-rust also serializes snapshot summaries from a `HashMap`, so the same
updates can serialize to different bytes.

## Decision

- `crates/catalog` runs iceberg-rust transactions only against a private
  capture catalog. Its `load_table` returns the pinned baseline, and its
  `update_table` stores the `TableCommit` and fails with a non-retryable
  error. The transaction therefore sees no newer base, applies its actions
  once, and sends nothing. Every other catalog method is unsupported.
- Each commit asserts the baseline's full requirement set: table UUID, `main`
  snapshot (absent for an empty table), current schema ID, last assigned
  field ID, default partition-spec ID, last assigned partition ID, and
  default sort-order ID. These are derived once from the baseline metadata.
  If an action's own requirement disagrees with them, the commit is
  refused before it is sent.
- A rewrite uses `RewriteFilesAction` with an explicit snapshot ID, the
  baseline `main` sequence number for added data files, file-existence
  validation, the publication ID as commit UUID, and summary properties
  `bergsmith.job-id`, `bergsmith.publication-id`, and
  `bergsmith.proposal-sha256`.
- The proposal hash covers the table UUID, job ID, publication ID,
  snapshot ID, the asserted baseline state, and the removed and added files,
  sorted by path. It is written into the snapshot summary, so it cannot
  cover the request that contains it.
- The serialized `CommitTableRequest` body is the durable artifact. It is
  hashed once (`bergsmith.catalog.v1.CommitTableRequest` domain), persisted,
  and replayed byte for byte, never re-serialized.
- Bergsmith's HTTP client sends a commit once. It does not follow redirects
  or use reqwest's retry of refused HTTP/2 streams. An `Idempotency-Key` is
  sent only if the catalog advertises `idempotency-key-lifetime`.
- Outcomes are classified as follows:
  - Any 2xx is committed.
  - 400, 401, 403, 404, 406, 409, and 419 are definitive rejections.
  - Every other status, a timeout, or a connection lost after sending is
    unknown.
  - A connection that was never established is an error: nothing was sent.
- Evidence lookup reloads the table and matches the snapshot ID, parent,
  manifest list, and all three summary properties. Absence never proves
  non-publication.

## Consequences

- There is still no iceberg-rust patch. AGENTS.md invariant 3 now forbids
  running `Transaction::commit` against a real catalog, rather than calling
  it at all.
- If an iceberg-rust upgrade changes how `Transaction::commit` drives its
  catalog, it can break the capture. Unit tests pin the behavior: a stale
  baseline keeps its original `main` snapshot requirement, added files keep
  the baseline sequence number, and an action that changes nothing is
  refused.
- Building a proposal writes manifests under the table's metadata location
  with the baseline's vended credentials. Unpublished manifests are left for
  orphan cleanup (M7).
- Lakekeeper v0.14.0 honors replays: the same request with the same key
  returns success without publishing twice.
