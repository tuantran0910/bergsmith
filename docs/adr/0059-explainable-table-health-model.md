# 0059. Explainable table health model

- Status: Proposed
- Date: 2026-10-26
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-028.T1

## Context

BS-028 needs a per-table health state that explains itself and can drive
scheduled maintenance. The statistics sampler (ADR 0057) already records the
raw shape of a table from metadata and manifests: file counts and sizes, a
size histogram, delete files and bytes by kind, manifest count and bytes,
partition counts and the busiest partitions, and the snapshot count. It never
reads rows, and it stores one immutable sample per observation time.

The plan's product research found no Iceberg-native health model. Maintenance
products build the same signals — small-file share, delete pressure, snapshot
backlog, manifest fragmentation, partition skew, and fresh statistics — and
publish vendor-heuristic thresholds, such as an average file size below
128 MB as a warning and below 32 MB as critical, or a delete-to-data ratio
above 0.1 as a warning and 0.5 as critical. Those numbers are copied from
research, not derived from Bergsmith's data, so the plan ships them as
documented, overridable defaults (plan §5, "Health thresholds").

Health has three consumers that must agree: the overview and table pages
(BS-027, BS-028.T4), health-triggered admission (BS-028.T3), and notifications
and metrics (BS-029, BS-030). Two implementations would drift. A stored verdict
would also drift: staleness depends on the clock, and a row written an hour ago
cannot say the statistics are six hours old.

## Decision

**Health is a pure function of one sample, a threshold set, and the current
time.** `bergsmith_maintenance::health::evaluate` takes the newest
`TableSample`, when the table's metadata was last confirmed, the effective
`HealthThresholds`, and `now`. It performs no I/O, reads no catalog or
storage, and never writes. The coordinator evaluates it when answering an API
request and passes its clock, so tests fix `now` instead of sleeping.

**Freshness is a confirmation, not the sample's age.** The sample is
append-only and unchanged metadata records nothing (ADR 0057), so a table whose
metadata has not moved can keep an old sample while every pass still reads it
successfully. The collector therefore records a per-table confirmation
timestamp on each successful read, and staleness measures the age of that
confirmation; the newest sample's time is only the fallback for tables sampled
before confirmations existed. There is no health column, no health
job, and no background recomputation: staleness ages with time, and storing a
verdict would let it disagree with the sample it describes.

**Findings are typed and reported only at warning or worse.** A `Finding` holds
the `FindingKind`, a `Severity` (`Warning` or `Critical`), the measured value
and the threshold it crossed in the finding's own unit, the threshold's
provenance (the defaults or a named policy), and a reason sentence composed
from those parts. A table's `HealthState` is the worst finding's severity;
`Healthy` means a sample exists and no finding fired. A table without a sample
is **not** healthy: the API reports no health and pages show "Not assessed",
because missing evidence is not evidence of health. This matches the overview's
never-sampled attention. Every finding names the sample it was measured from
and the confirmation it relies on, so a table whose metadata advanced after the
sample is described as the sampled snapshot rather than the current one.

**Six findings, each with one fixed measurement and documented defaults.**

| Finding | Measured | Warning | Critical | Evaluated when |
|---|---|---|---|---|
| `small_files` | Average live data-file size, bytes | below 128 MiB | below 32 MiB | at least 2 data files |
| `delete_pressure` | Live delete bytes in per-mille of live data bytes | above 100‰ | above 500‰ | delete bytes and data bytes are both nonzero |
| `snapshot_backlog` | Snapshots retained in the table's metadata | 1,000 or more | 5,000 or more | always, given a sample |
| `manifest_fragmentation` | Live data files per data manifest, rounded down | below 1,000 | below 100 | at least 2 data manifests and 100 data files |
| `partition_skew` | Busiest partition's share of live data files, per-mille | 500‰ or more | 800‰ or more | at least 2 partitions and 100 data files |
| `stale_statistics` | Age of the last successful metadata confirmation | 1 hour or more | 6 hours or more | a confirmation exists (the sample's time falls back) |

The file-size and delete-ratio defaults are the researched vendor heuristics.
The snapshot critical bound is five times the researched warning, because
1,000 retained snapshots is already unusual and 5,000 threatens metadata
handling. Manifest fragmentation uses data files per data manifest, excluding delete
manifests, which cannot hold data files: Iceberg's default manifest target is
8 MiB, which holds thousands of entries, so a data manifest with fewer than
1,000 entries is small and one with fewer than 100 is severe. The skew defaults
flag a table where one partition holds half (warning) or four fifths (critical)
of the data files; the sample stores the busiest partition's file count itself,
so a display bound that hides the partition list cannot hide the skew. Staleness defaults to 12 and 72 times the
default five-minute sampling interval. The "evaluated when" guards keep a
seeding or single-file table from being reported as unhealthy for being small;
they are part of the model, not policy, and their boundaries are tested.

**Comparisons are integer and ratios are cross-multiplied.** No finding uses a
floating-point value, in comparisons or in the payload. Delete pressure
compares `delete_bytes * 1000` with `threshold * data_bytes` in `u128`;
partition skew compares `busiest.files * 1000` with `threshold * data_files`
the same way. A measured per-mille value is divided once, rounding down, only
for display, so two evaluations of one sample serialize identically. Findings
are ordered by kind in the order above; ties are impossible because each kind
is emitted at most once. The table state and the primary finding (the first
worst-severity finding) are therefore deterministic.

**Thresholds are code defaults, overridable per policy.** `HealthThresholds`
carries an optional warning and critical value per finding, in that finding's
unit; absent fields keep the defaults. Validation rejects a zero value and a
critical that is not strictly more severe than its warning (smaller for the
below-findings, larger for the above-findings), so a stored override cannot
invert the ladder. The type lives in `bergsmith_common` beside `PolicySpec`,
because a policy references it and `bergsmith-common` depends on no other
workspace crate; `bergsmith_maintenance` owns the statistics and the
evaluation. A policy's overrides govern its own trigger evaluation (T3). For a
table with one or more enabled schedules, the page reports the worst evaluation
across their threshold sets, with each finding naming the policy that supplied
its threshold, so the displayed state is never calmer than a trigger would act
on; without an enabled schedule the defaults apply.

**The API carries the typed parts, not a prose verdict.** Counters cross the
API as decimal strings, as every other counter does (ADR 0057), timestamps are
RFC 3339 in UTC, and the reason sentence is composed from the same parts the
fields carry. Tests assert the kind, severity, measured value, threshold, and
guard boundaries; the sentence is not the contract. The versioned resource
contract and the page presentation are T4's, layered on this model.

This task (T1) lands the pure model, the defaults, and their exhaustive tests.
The policy column, the override API, and the trigger plumbing are useless
before T3 exists and land with it, using the same typed threshold set.

## Consequences

- T2 maps the actionable findings onto a prefilled preview (`small_files` and
  `delete_pressure` to compaction, `partition_skew` advisory until the planner
  can target a partition) with the planner's estimate; findings that only M7
  operations address stay advisory.
- T3 evaluates the same function at occurrence admission with the policy's
  thresholds and records the triggering kind and severity on the job, so every
  triggered run explains its cause (the M6 gate). Windows, budgets, and
  deduplication are unchanged.
- T4 computes health from the newest sample per table, on read, for the Tables
  list's state column and the Health tab. Filtering and sorting by state are
  server-side; no schema stores a state.
- BS-029 alerts on a table turning `Critical`; BS-030 exposes counts per state.
  Both consume the same evaluation, so an alert and a page cannot disagree.
- The defaults become a documented operating contract; changing one is an ADR
  or support-matrix-visible change, and the plan's open threshold decision is
  revisited with statistics history.
- Delete pressure uses bytes of both delete kinds. Row counts do not feed it:
  the plan records that descriptor counts do not establish deleted-row ratios,
  and a small delete file can carry many rows.
- Freshness lives in its own ledger row per table, upserted forward-only, so a
  retried pass cannot make a table look older and the sample history stays a
  history of changes rather than of checks.
- Health never triggers a job on its own. Admission stays behind the schedule,
  window, budget, and grant checks in BS-019 and BS-020.

## Alternatives considered

- **Store the evaluated state with each sample.** Cheap to read, but staleness
  is a function of the clock and a stored verdict silently becomes wrong as the
  sample ages; it also cannot reflect a threshold change without rewriting
  history.
- **Evaluate in the web client.** The server must own triggers and list
  filters, so a second implementation would drift and could authorize a run the
  page calls healthy.
- **Floating-point ratios.** Rounding at a boundary, serialization, and
  cross-language comparison all become ambiguity; integer cross-multiplication
  has none.
- **One organization-wide threshold set.** The plan calls for per-policy
  overrides, and a table with a strict commit cadence may need to act earlier
  than the fleet default.
- **Two thresholds per finding on the table itself.** This splits operating
  configuration across policies and tables and leaves trigger evaluation with
  a second source; policy overrides keep one place per decision.
