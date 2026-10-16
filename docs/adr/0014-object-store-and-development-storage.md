# 0014. Object store over FileIO, and RustFS for development

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2 Object storage layer, §5 Security and deployment

## Context

ADR 0006 planned an `ObjectStore` trait on OpenDAL for operations `FileIO`
might lack. BS-002 and BS-005 showed that `FileIO` already lists, stats, and
deletes with directory semantics, and that OpenDAL exposes no public API to
list or abort incomplete multipart uploads. The development store had to
support the STS flow Lakekeeper vends credentials from.

## Decision

- `ObjectStore` is a struct over iceberg-rust `FileIO`, not a trait.
  `ObjectStore::open` is the single place that turns a storage profile and a
  credential cell into a backend; GCS and Azure are added there, behind the
  same contract suite.
- Workers use a `CredentialCell`, an iceberg-rust `StorageCredentialProvider`,
  so refreshed credentials apply to open readers and writers without
  rebuilding `FileIO`. Ambient credentials and instance metadata are disabled.
- Incomplete multipart uploads are cleaned up by a bucket lifecycle rule
  (`AbortIncompleteMultipartUpload`), a documented deployment requirement.
  Writers abort their own uploads on failure; the rule covers crashes.
- RustFS 1.0.1 is the Compose store. BS-005 verified that it serves STS
  `AssumeRole`, that Lakekeeper v0.14.0 vends table-scoped credentials from
  it, that writes under `<table>/bergsmith/<job>/<attempt>/` succeed, and that
  writes outside the table are denied.

## Consequences

- Fewer moving parts: one storage abstraction, tested by one contract suite
  on memory, the local filesystem, and RustFS.
- Bergsmith cannot itself list leftover multipart uploads; operators must
  configure the lifecycle rule, and the Compose buckets have it.
- Backend listings may return paths in a collapsed form (`memory:/…`,
  `file:/…`); `ObjectStore` normalizes them back to `Location`s.
