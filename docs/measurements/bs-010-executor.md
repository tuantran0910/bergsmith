# BS-010 native executor measurements

Measured 2026-10-17 on Darwin 25.3.0, arm64, with Rust 1.94.1 and the workspace's
pinned iceberg-rust revision `f501ff0c245b4b625fe52feaf6125c69f7132557`. Both driver
and worker used the release profile (thin LTO). Source and output storage was
the local filesystem; operating-system caches were not flushed.

The driver generates real fixtures, prepares immutable plans, and launches one
fresh worker process per sample under `/usr/bin/time -l`. Fixture generation and
independent oracle validation occur in the driver, outside the measured worker.
Elapsed milliseconds come from the worker's local-run summary and include
metadata loading, preparation, footer preflight, reading, writing, close and
receipt persistence. Peak RSS is the operating system's maximum resident set
size for the worker. This is a measurement, not a process-memory enforcement test.

Each benchmark plans the whole fixture table, so rows for shared-delete, old-spec
and evolved-schema tables include files outside the spike's original selected
group. The separate native rewrite acceptance tests exercise those selected
groups and verify untouched files byte for byte.
The harness sets `min_input_files=1` and `min_files_removed=0` to exercise tiny
fixtures; all other planner limits and execution budgets use their defaults.

| Fixture | Input → live rows | Elapsed ms (three runs) | Maximum RSS (MiB) | Planner estimate (ms) |
|---|---:|---|---:|---:|
| Small files, duplicates and nulls | 40 → 40 | 45, 11, 12 | 25.19 | 2001 |
| Position deletes across row groups | 24 → 18 | 13, 11, 12 | 27.69 | 2001 |
| Equality deletes, old and same sequence | 8 → 6 | 14, 9, 12 | 31.77 | 2001 |
| Nullable single key | 3 → 1 | 12, 12, 10 | 27.70 | 2001 |
| Nullable composite keys | 7 → 3 | 12, 12, 10 | 29.98 | 2001 |
| Shared deletes | 24 → 18 | 12, 11, 11 | 27.81 | 2001 |
| Fully deleted group | 5 → 0 | 6, 7, 7 | 27.31 | 2001 |
| Original partition specs | 20 → 19 | 32, 30, 33 | 33.94 | 3335 |
| Renamed and added columns | 4 → 4 | 13, 11, 10 | 27.41 | 2001 |
| 50,000 equality keys | 100,000 → 50,000 | 938, 926, 911 | 113.00 | 2037 |
| Wide rows, 64 MiB of payloads | 32,768 → 32,768 | 22, 23, 22 | 35.56 | 2015 |

All 33 samples passed exact independent row-multiset checks and receipt hash
verification. The zero-output fixture wrote no data files. Every measured job
completed below its planner estimate and the 180,000 ms duration budget. The
50,000-key predicate path needs no iceberg-rust patch for this acceptance case.

The existing 64 MiB/s planner throughput and two-second task overhead remain
unchanged: overhead covers these small fixtures, including the CPU-heavy delete
case. These samples do not establish 64 MiB/s for large equality-delete workloads,
validate the 2 GiB group default, or measure remote-storage latency. Do not
extrapolate a process memory envelope from the compressed input file size.
Linux cgroups, distributed concurrency and broader deletion semantics remain
separate acceptance work.

Raw samples, including input/output bytes and per-run RSS, are retained in
[bs-010-executor.json](bs-010-executor.json).

Reproduce with a new output directory on macOS or Linux (`/usr/bin/time` required):

```sh
make bench-executor BENCH_DIR=.private/executor-benchmark-new BENCH_ITERATIONS=3
```

The driver refuses existing directories and never cleans up earlier measurements.
For a single fixture, build the targets and invoke the example directly:

```sh
cargo build --release --locked --bin bergsmith-worker
cargo run --release --locked -p bergsmith-maintenance --example executor_bench -- \
  --worker target/release/bergsmith-worker \
  --directory .private/large-equality-benchmark-new \
  --fixture large_equality_delete --iterations 3
```
