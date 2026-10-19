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

BS-015 adds `TaskGrant.metadata_location` without changing revision 2 or hashed
messages. It identifies the original planning metadata file. Existing peers can
ignore it; the new worker refuses an omitted location before execution. The
same-build local `GetTaskResponse` also carries that location, artifact digest,
and non-secret worker-pool S3 routing settings.

`TaskGrant.max_manifest_entries` carries the immutable plan's preparation
inventory bound, and local `GetTaskResponse` forwards it to the subprocess.
Zero in an older grant retains the historical 1,000,000-entry default. Older
workers can ignore a raised bound and fail conservatively at their default;
using raised bounds requires upgraded workers. This additive revision-2 field
changes neither hashed artifacts nor immutable resource envelopes.

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
