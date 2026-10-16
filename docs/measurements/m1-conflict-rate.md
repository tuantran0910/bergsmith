# M1 conflict-rate measurement

- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §1 Existing-catalog integration and conflict policy; BS-007.T6
- Tool: `crates/catalog/examples/conflict_rate.rs`. It runs against the Compose Lakekeeper v0.14.0 and RustFS 1.0.1. It is run by hand, not in CI.

## Question

Under conservative validation, a job succeeds only if it plans, executes,
and publishes between two commits of the table's writer. With a random start,
the success rate should be about `1 - d/T` for a job of duration `d` and a
writer commit interval `T`. Commit-aligned starts should raise it to nearly
100% while `d` stays inside `T`. Does the commit adapter behave that way
against a real catalog?

## Setup

- **Writer:** a synthetic sink, not RisingWave, by decision for this
  milestone. It appends one data file per commit through the standard REST
  commit, sleeps `T`, and retries after a conflict, as a streaming sink does.
- **Jobs:** model maintenance jobs that run one after another.
  1. Pin the current table and rewrite the newest live sink file through the
     real adapter, so manifests are written to RustFS.
  2. Wait `d`, standing in for execution.
  3. Run the reload check (`Baseline::changes`), and send only if it is
     clean.
- **Start modes:**
  - aligned: start when Inventory sees a new `main` snapshot (100 ms poll);
  - random: start after a uniform delay in `[0, T)`.
- **Scale:**
  - T = 5 s instead of the production 5 minutes. Outcomes depend on `d/T`,
    and catalog round trips (tens of ms) stay around 1% of `T`.
  - 10 jobs for each mode and `d/T` value, about 5.5 minutes in total.

```text
cargo run -p bergsmith-catalog --example conflict_rate -- \
    --interval-secs 5 --fractions 0.25,0.5,0.75 --jobs 10 --poll-ms 100
```

## Results

| Start | d/T | Jobs | Published | Caught by reload check | Rejected by catalog | Success | `1 - d/T` |
|---|---|---|---|---|---|---|---|
| aligned | 0.25 | 10 | 10 | 0 | 0 | 100% | 75% |
| aligned | 0.50 | 10 | 10 | 0 | 0 | 100% | 50% |
| aligned | 0.75 | 10 | 10 | 0 | 0 | 100% | 25% |
| random | 0.25 | 10 | 9 | 1 | 0 | 90% | 75% |
| random | 0.50 | 10 | 9 | 1 | 0 | 90% | 50% |
| random | 0.75 | 10 | 1 | 9 | 0 | 10% | 25% |

The sink retried 3 commits that a published rewrite had beaten.

## Findings

1. **Commit-aligned starts remove conflicts while `d < T`.** All 30 aligned
   jobs published, including those using 75% of the interval. This supports
   the MVP's two mitigations together: commit-aligned start and a duration
   budget well inside the observed interval.
2. **Random starts conflict roughly as `d/T` predicts.** At 10 jobs per point
   the rates are noisy, with ±15% sampling error. The `d/T = 0.5` point
   (9/10, against 5/10 expected) has about a 1% chance by sampling alone, so
   it should be rerun with more jobs before it is quoted. The trend is clear:
   longer jobs conflict more without alignment.
3. **Every conflict was caught before sending.** No stale commit reached the
   catalog. The reload check found each change, and the catalog's
   requirements remain the guarantee for the race after it, as the conflict
   fixtures show.
4. **Maintenance does not block the writer.** When a rewrite published first,
   the sink's next commit failed its requirement once and landed on retry.

## Limits

- The writer is synthetic. RisingWave's commit timing, file sizes, and
  delete files are not modeled. Rerun with a real sink before quoting rates
  for a deployment.
- Jobs are models: execution is a fixed sleep, not BS-010's executor.
  Repeat the measurement once real jobs exist, at the production 5-minute
  interval.
- The sample is small. For a quoted rate, use at least 50 jobs per point.
