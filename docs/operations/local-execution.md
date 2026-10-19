# Local execution

The local worker executes an immutable plan on filesystem storage without a
coordinator, PostgreSQL or a catalog connection. It applies deletes, writes
replacement Parquet files and saves receipts. It does not publish table metadata.

Build and run:

```sh
cargo build --release --locked --bin bergsmith-worker
target/release/bergsmith-worker local \
  --plan /absolute/path/plan.json \
  --plan-sha256 'replace-with-trusted-plan-sha256' \
  --receipts /absolute/path/new-receipt-directory
```

Produce `plan.json` with `bergsmith_maintenance::Planner` and `PlanDocument::to_json`.
Obtain its trusted digest separately with `PlanDocument::sha256`; do not trust a
digest downloaded alongside an untrusted plan. The local worker checks the plan,
task hashes and complete planning baseline before execution. It reads the pinned
metadata location recorded by the plan and never reloads a catalog head. Local
mode currently requires filesystem metadata and data locations. Configuring S3
vended credentials belongs to distributed worker wiring.

The receipt directory's parent must exist. The directory itself must be new.
It contains `<task-ordinal>.pb` files encoded as
`bergsmith.artifact.v1.TaskReceipt` and a final `summary.json` with receipt hashes,
row/byte counts and elapsed milliseconds. A successful log line means execution
completed; outputs remain unpublished. Empty-output tasks still produce receipts.

Each task receives a fresh attempt ID and writes under
`<planned-data-location>/bergsmith/<job-id>/<attempt-id>/`. Existing files are
refused. Failures leave partial outputs and earlier receipts available for
inspection, with no final successful run summary. Source and delete files are
never removed. Local mode does not retry failed tasks automatically.

Use `--limits /absolute/path/limits.json` to provide a fixed execution envelope.
Omitted fields use these defaults; unknown fields and zero budgets are rejected:

```json
{
  "batch_rows": 1024,
  "batch_memory_bytes": 33554432,
  "delete_memory_bytes": 268435456,
  "decode_row_group_bytes": 134217728,
  "read_request_bytes": 134217728,
  "writer_row_group_bytes": 8388608,
  "footer_bytes": 16777216,
  "object_requests": 4,
  "output_bytes": 4294967296,
  "output_files": 10000,
  "io_timeout_ms": 30000,
  "duration_ms": 180000,
  "manifest_entries": null
}
```

`manifest_entries` bounds the live manifest entries preparation reads. When it
is `null` or omitted, local mode uses the plan's recorded
`max_manifest_entries`. `read_request_bytes` also bounds the pinned metadata
file. Failures are logged with their stable `reason_code` and suggested
`disposition`.

Delete memory is a conservative admission reservation from footer sizes and key
counts. Decoded batches are checked by retained Arrow memory. These component
budgets do not impose a total process RSS cap. Distributed process supervision
and Linux hard process memory enforcement are available through the
[distributed worker runtime](workers.md) (BS-015). Native blocking work cannot be
assumed to stop when a future's deadline expires. The executor does not sort or
spill.

Preparation, storage operations and whole attempts also check elapsed time when
work completes. A completion that overruns its deadline cannot return a successful
receipt, even if CPU or blocking work delayed polling the timeout timer.

For reproducible fixtures, oracle validation and isolated worker RSS measurements,
see [the executor benchmark](../measurements/bs-010-executor.md).
