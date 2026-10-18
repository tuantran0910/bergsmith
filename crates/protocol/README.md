# bergsmith-protocol

Wire contracts for Bergsmith, compiled from `proto/` by `build.rs` with
`protox` (no system `protoc` needed).

| Package | Purpose |
|---|---|
| `bergsmith.common.v1` | Shared enums and the typed `Failure` |
| `bergsmith.artifact.v1` | Hashed task artifacts and receipts |
| `bergsmith.cluster.v1` | Coordinator–worker `ClusterService` |
| `bergsmith.worker.local.v1` | Supervisor–subprocess `LocalTaskService` |

## Compatibility rules

`bergsmith.cluster.v1` must keep working between a coordinator and workers
of adjacent releases. `make proto-breaking` (buf `WIRE_JSON` rules) enforces
the mechanical part in CI; the rest is review discipline.

- Never change a field's number or type, and never reuse a number. Delete a
  field only by marking its number and name `reserved`.
- Add fields with new numbers. A reader that does not know a field ignores
  it, so new fields must be optional in meaning: an old peer that omits them
  still gets correct behavior.
- Enums start with `*_UNSPECIFIED = 0`. Add values only at the end. Readers
  reject `UNSPECIFIED` and unknown values (`convert` returns an error) rather
  than guessing.
- A change that old peers cannot ignore gets a new `protocol_revision`,
  negotiated in `RegisterWorker`, or a new package version (`v2`).
- `bergsmith.worker.local.v1` connects two processes of the same build, so it
  may change freely between releases; it still follows the numbering rules so
  buf can check it.

## Worker protocol revision

The BS-013 worker service negotiates cluster protocol revision **2**. Revision 2
adds `RequestTaskResponse.disposition`: a repeated poll whose original attempt
ended returns its durable disposition rather than another task. Revision 1
workers must upgrade before registration; an unknown response variant must never
be interpreted as permission to execute or to retry execution with a fresh poll.

## Hashed messages

`TaskArtifact` and `TaskReceipt` are identified by SHA-256 digests
(`hashing`), so their messages also follow these rules:

- No `map` fields and no floating-point fields; a test walks the descriptors
  to enforce this.
- Repeated fields are sorted by the key documented on each field before
  hashing; `artifact_digest` and `receipt_digest` sort a copy themselves.
- Adding a field keeps existing digests stable while the field is unset.
  Golden-digest tests fail on any change to the encoding; treat that failure
  as a breaking change, not a test to update.

## Secrets

`StorageCredentials` and `ServiceToken` have redacted `Debug`
implementations. Never log their values or copy them into artifacts,
receipts, or errors.
