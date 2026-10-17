# 0021. Pin the per-spec manifest dependency patch

- Status: Accepted
- Date: 2026-10-17
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Pinned integration baseline; §4 BS-011.T4

## Context

ADR 0020 records an upstream defect: replacement files under a known historical
partition spec are rejected, and added manifests always use the default spec.
The maintainer published the correction in
[risingwavelabs/iceberg-rust#262](https://github.com/risingwavelabs/iceberg-rust/pull/262).
Its head extends the existing integration baseline without unrelated changes.

Java Iceberg resolves each added file's declared spec and writes added files in
groups by spec ID. Its rewrite tests explicitly select historical output specs.
The correction preserves that behavior and keeps data sequence numbers explicit.

## Decision

- Keep the original integration baseline
  `f501ff0c245b4b625fe52feaf6125c69f7132557` in workspace dependency declarations.
- Redirect the core, REST catalog, and OpenDAL storage packages together using
  Cargo's `[patch]` mechanism to
  `https://github.com/thealtoclef/rw-iceberg-rust.git`, revision
  `eb162a1e109dbd2961d1fcd3e93cc8c770120bb9`.
- Allow this Git source in `deny.toml` and commit its resolved lockfile entries
  when BS-011 is committed. Never depend on a branch name or a private checkout.
- The correction validates added files against their own registered partition
  spec, rejects unknown IDs and incompatible values, and writes added data and
  delete manifests per spec. Preserve snapshot and sequence-number semantics.
- Record the patch source, upstream PR, evidence, and removal condition in
  [dependencies](../dependencies.md). Advance the pin only through a reviewed
  dependency update; upstream PR changes do not automatically enter Bergsmith.

## Consequences

Old-spec native replacements can be prepared through the capture catalog while
the current default spec and original REST requirements stay unchanged.
All workspace consumers use a single patched iceberg-rust source revision.

The patch is not yet merged upstream. Once an integration revision contains the
fix, move all three packages together to it and remove the redirect. Keep
the old-spec and sequence-number acceptance tests through that transition.
