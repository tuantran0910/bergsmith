# 0026. Durable worker supervision and platform resource enforcement

- Status: Accepted
- Date: 2026-10-18
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-015

## Context

Native rewriting must not prevent lease renewal or credential refresh. A worker
can also lose a result acknowledgment after successful execution. Repeating that
execution would create unnecessary outputs and could exceed the attempt budget.
ADR 0008 chooses local gRPC; this decision specifies its runtime and recovery.

## Decision

- Run registration recovery, polling/execution, heartbeats, and reporting as
  independently polled futures. Reserve one execution slot. Retry a lost poll
  using its original UUID; never treat an ended assignment as a new grant.
- Verify the downloaded artifact digest and grant identities before starting a
  same-build `exec` subprocess. Add the original planning metadata location to
  the grant; a worker refuses a grant missing that location. Workers read that
  immutable metadata through storage credentials and never load a catalog head.
- Serve local gRPC from a private per-attempt directory. `GetTask` provides
  verified input and non-secret storage routing. `WatchControl` retains both
  sticky cancellation and the latest credentials, so slow readers cannot lose
  either. FileIO readers and writers share the refreshable credential provider.
- Persist a result or typed failure atomically and sync it before acknowledging
  its local RPC. The blocking writer owns the completion guard until both the
  durable record and completion state are settled, even if the RPC disappears.
  A published record prevents an opposite outcome even when directory syncing
  fails. Journal requests include the original worker session and
  immutable receipt digest, never credentials. Retry that exact request until
  the coordinator gives an authoritative outcome. Retain acknowledged entries
  for inspection; exclusively lock each worker state directory.
  Recover an ordered pending index once at startup, then update it after local
  persistence and coordinator acknowledgment. Routine polling never traverses
  acknowledged history. Conflicting pending completion files fail recovery.
- Stop polling while reports remain unacknowledged. Keep heartbeats independent
  of execution and reporting. Restarting may replay reports from a fenced
  session; it never executes them again. Attempt expiry remains authoritative
  at the coordinator.
- Cancellation first updates the control stream, then signals the process group
  with SIGTERM after a bounded grace period, then SIGKILL two seconds later.
  Shutdown, lost leases, and fencing use the same path. Dropping supervision also
  kills the process group. An exit without a local completion report is
  `TaskSubprocessCrashed`, eligible for the existing three-attempt retry policy.
  Local shutdown records `WorkerExecutionInterrupted` as retryable before
  sending control, so a child's cancellation report cannot change that
  classification. The coordinator still enforces the execution retry budget.
  Rejected failure reports resolve through `GetAttemptStatus`; only a known
  ended disposition acknowledges the journal.
- On Linux require delegated cgroup v2 memory, cpu, and pids controllers. Attach
  a single-threaded bootstrap to the configured cgroup and re-exec the task
  before starting its runtime. Replacing the bootstrap address space releases
  its allocations made outside the task cgroup; migrating alone would retain
  those original memory charges. Arm `PR_SET_PDEATHSIG` before runtime threads.
  Launch from a dedicated thread retained for the child's lifetime, because
  Linux parent-death protection follows the creating thread. Set memory/swap
  and CPU ceilings before launching the bootstrap. Missing delegation fails
  closed. Record OOM evidence as a permanent resource failure.
- Apply CPU-time and core-dump rlimits in the subprocess. On development systems
  outside Linux, limit additional virtual data growth and sample the task's RSS
  from the independent supervisor every 100 ms. Darwin reserves large allocator
  arenas before execution, so an ordinary task-sized address-space rlimit can
  be rejected even when RSS is small. RSS sampling can overshoot between samples
  and is not a production memory-isolation guarantee; Linux cgroups provide it.
- Expired vended credentials stop the attempt with
  `StorageAuthorizationExpired`, eligible for execution retry. Deterministic
  memory, CPU, and duration failures remain permanent.

## Consequences

Workers need durable private local state and, in production Linux deployments,
explicit cgroup delegation. The supervisor does not send its service token,
environment, catalog access, or database access to task subprocesses.

The cluster field addition is wire compatible. Existing peers may ignore it;
the new runtime rejects an omitted metadata location before execution. This
does not alter artifact or receipt hashing. Progress reports carry finalized
row counts and output inventory; running attempts may show zero counters until
native execution completes. Prefix inventory is already registered with grants.

The BS-016 distributed failure-injection suite and release-scale test remain the
gate for certification of the complete distributed maintenance lifecycle.
