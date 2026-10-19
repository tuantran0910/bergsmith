# 0028. Executor failure classification and preparation bounds

- Status: Accepted
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2 Failure and retry dispositions; §4 BS-010
- Supersedes: the preparation-timeout disposition and fixed inventory bound in ADRs 0018 and 0019; their other decisions remain in force

## Context

Review of the executor found that preparation always used the default planner
inventory bound, whatever bound the plan was built with. A preparation
timeout (60 seconds of metadata reads) was permanent although each storage
operation timeout suggests a retry, and the plan classifies temporary
object-store failures as `RetryTask`. Metadata whose data root differed from
the planned output prefix failed as `InvalidOutputPrefix` rather than as a
baseline mismatch. Local mode read pinned metadata without the late-completion
deadline or a size bound, and logged failures without their reason code.

## Decision

- `ExecutionLimits::manifest_entries` bounds preparation's inventory. Unset, it
  is the planner default; local execution sets the plan's recorded
  `max_manifest_entries`. Distributed grants carry the bound in additive
  `TaskGrant.max_manifest_entries`, persisted before delivery and forwarded
  through local `GetTaskResponse` to the subprocess. Zero in older grants
  keeps the historical 1,000,000-entry default. Older workers ignore the new
  field and may fail conservatively on larger inventories; upgrade them before
  using raised bounds. Artifact hashes and resource envelopes are unchanged.
- `PreparationTimeLimitExceeded` suggests `RetryTask`, and the coordinator
  classifies it as an execution retry within the three-attempt budget.
  Preparation reads only metadata bounded by the inventory limit, so an overrun
  indicates slow storage. `TaskDurationExceeded` and other resource-envelope
  failures stay permanent.
- A well-formed attempt prefix under a different data root than the supplied
  metadata's is a `PlanningBaselineMismatch` (`ReplanRequired`). Any other
  prefix mismatch stays `InvalidOutputPrefix`.
- Worker metadata reads use the same late-completion deadline as execution.
  Local mode refuses metadata larger than `read_request_bytes`, and logs each
  failure's reason code and disposition.
- Preparation still reads manifests twice: once for Bergsmith's identity and
  delete checks, once through the native scan planner. The native manifest
  cache is crate-private, and the second read is what the exact attachment
  check compares against.

## Consequences

Plans built with a non-default inventory bound execute locally. Slow metadata
reads use the retry budget instead of failing jobs. Tests drive duration and
storage deadlines through `execute` with storage that never answers under a
paused clock.
