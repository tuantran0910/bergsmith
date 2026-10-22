# Worker sessions and task grants

The coordinator `serve` runtime ([serving](serving.md)) exposes the worker
endpoint over TLS. It plans and registers each immutable task artifact before
the scheduler offers it.

The coordinator library exposes `workers::WorkerService`, implementing
`bergsmith.cluster.v1.ClusterService`. Serve it with tonic's
`ClusterServiceServer` over `worker_tls_config(identity)`. Every RPC rejects a
connection without TLS transport information and a valid pool token. Configure
only token verifiers and their organization, service principal, and pool; never
persist the bearer token. Token provisioning and audit commands arrive in BS-017.

Workers must support cluster protocol revision 2 and advertise exactly one
executing task. Default timings are a 15-second heartbeat, a 90-second session
and attempt lease, and a 30-second long poll. CPU and memory capacity must cover
the offered task's execution envelope; capability names must be sorted and unique.
Worker names identify instances within a principal and pool: restarting with the
same name fences the old session. Use different names for distinct instances.

Use a fresh UUID for each new poll. After a timeout or lost response, repeat that
UUID until its original result is known. A completed empty poll stays empty even
when work arrives later; start a new poll after receiving `NoTask`. An ended
assignment returns `disposition`; do not execute it or interpret it as a new grant.
An unavailable reporting RPC requires acknowledgment recovery, not re-execution.

Grants are committed with their output-prefix inventory before being returned.
They contain a fetchable artifact URI and canonical digest, original input-state
identity, generation, lease deadline, resource envelope, and current table-scoped
credentials. Only safe grant fields are persisted. `CatalogWorkerAccess` reloads
catalog metadata and vends credentials into memory; configure its job bindings
from the original durable job/catalog identity. Catalog credentials never reach
the worker. Heartbeats return renewal, cancellation, or fencing instructions, and
refresh the worker's storage credentials only when the delivered ones expire
within one attempt lease (or their expiry is unknown, such as after a coordinator
restart). A failed refresh never fails the heartbeat: the committed renewal is
still returned, and the worker keeps its current credentials. If they lapse,
storage access fails with an authorization error classified as `RetryTask`.
Every rejected worker RPC carries a typed `Failure` detail with a stable reason
code and disposition. For development storage with no vended credentials, the
coordinator caches that absence for at most one lease. Expired hints are pruned
on later deliveries; losing a hint only causes another access lookup.

The first admitted offer binds the task's memory, CPU, duration, and output-byte
limits durably. Every retry must use those same limits, including after a
coordinator restart. Changing the resource envelope requires a new plan and job.
Dispatch uses per-task reservations, so a blocked grant cannot hold the queue
mutex. A grant-decision timeout requires repeating the same poll UUID.

Report exactly the same immutable receipt until `Accepted` or `AlreadyAccepted`
is known. Acceptance means the receipt and finalized inventory are durable; the
table changes only after coordinator publication. `StaleAttempt`, `Cancelled`,
and `InvalidResult` are authoritative refusals. `GetAttemptStatus` resolves lost
acknowledgments. Draining stops new grants and keeps existing leases renewable.

Malformed output locations and decoding errors receive `InvalidResult`.
Retryable native execution and metadata-read failures carry explicit transient
reason codes; they may retry within the execution budget. Suggested worker
dispositions cannot make an invalid output or resource-limit failure retryable.
See [ADR 0024](../adr/0024-worker-receipt-fencing-and-task-envelopes.md).

BS-013 provides the RPC service and explicit task-offer interface. BS-014 adds
[durable admission, retry timers, and queue reconstruction](scheduling.md);
BS-015 provides worker runtime loops and subprocess enforcement. A drained or expired worker is not
proof that its process has stopped writing. Preserve its registered output prefix
under the retention rules in [publication.md](publication.md).

The TLS RPC test suite generates ephemeral certificates with OpenSSL; OpenSSL
must be available in development and CI. No private test key is checked in.

## Running a supervisor

Run `bergsmith-worker run --config /etc/bergsmith/worker.json --token-file
/run/secrets/worker-token`. The configuration is JSON with these fields:

```json
{
  "endpoint": "https://coordinator.example:7443",
  "ca_certificate": "/etc/bergsmith/coordinator-ca.pem",
  "worker_name": "worker-1",
  "pool": "default",
  "capabilities": ["native-v2"],
  "memory_bytes": 2147483648,
  "cpu_millis": 1000,
  "state_directory": "/var/lib/bergsmith/worker",
  "cgroup_root": "/sys/fs/cgroup/bergsmith-worker",
  "s3_region": "us-east-1",
  "s3_endpoint": "https://objects.example",
  "s3_path_style": true,
  "journal_retention_hours": 720
}
```

Use the coordinator's configured capability profile names, sorted without
duplicates. Omit `ca_certificate` to use system roots. Omit `s3_endpoint` for AWS.
Omitting `s3_region` selects filesystem storage for development. These are
connection settings; storage authentication always comes from grants and
heartbeat refreshes. The pool token file contains a `bsw1` service token and
should be readable only by the worker account. No secret belongs in this JSON.
`journal_retention_hours` is optional: acknowledged journal records are kept
for 720 hours (30 days) by default, and at least one hour (see
[Journal retention](#journal-retention)).

The state directory's parent must exist. The worker creates the directory with
mode `0700`, refuses a public directory, and takes an exclusive lock. Use a
short path: per-attempt UUID directories and `task.sock` must fit the operating
system's Unix socket path limit. The same directory must survive process
restarts. Never share it between concurrently running worker instances.

Linux requires a writable, delegated cgroup v2 subtree with the `memory`, `cpu`,
and `pids` controllers enabled for child cgroups. The service manager or container
runtime must provide that delegation; the worker does not change host controller
configuration. Each task gets `memory.max`, zero swap, `cpu.max`, and a 128-PID
ceiling before local `GetTask` is served. Missing controllers or failed limit
installation refuse execution. The development Compose stack exposes an opt-in
`worker` profile (`make app-worker`); it uses a privileged container and the
host cgroup namespace for convenience. Production delegation and packaging
acceptance remain BS-020.T4.
The single-threaded bootstrap joins that cgroup and re-executes before starting
the native task runtime, so startup heap allocations are charged to its budget.
Its dedicated launching thread remains alive for the child process's lifetime.

Development platforms use CPU/core rlimits, a bound on additional virtual data
growth, and 100-ms RSS sampling. A sampled RSS bound can overshoot between
samples and does not provide Linux's hard memory isolation. Three consecutive
failed RSS samples stop the task with retryable `ProcessMemoryUsageUnavailable`.
CPU rlimits bound total CPU seconds; only Linux also limits CPU bandwidth, so
development platforms size the CPU-time backstop for every available core over
the task duration, and the duration limit is what stops a busy task. See
[ADR 0026](../adr/0026-durable-worker-supervision.md).

At startup the Linux worker removes empty `bergsmith-<pid>-*` cgroups left by an
earlier run. A cgroup that still has members is kept and logged, never killed.
Each task cgroup is killed through `cgroup.kill` and removed after its members
exit, including after a partial setup failure.

The worker binary launches `exec --socket <path> --parent-pid <pid>` itself.
Subprocess environments are cleared, and standard input is a null device. The
subprocess's standard output and error are drained continuously and forwarded
into the supervisor's log under target `bergsmith_worker::task` with the attempt
ID, one bounded line (8 KiB) at a time. The subprocess holds no service token
and does not log its storage credentials.
Tokens and storage credentials are not passed on command lines. The private
socket supplies only the task's storage credentials. Storage credential refresh
updates existing readers and writers; expiry stops the attempt. Running progress
counters are currently finalized when the native result completes.

SIGINT or SIGTERM stops admission and requests cooperative cancellation. The
supervisor escalates to SIGTERM after the grace period and SIGKILL two seconds
later, targeting the entire task process group. Lease expiry and fencing also
stop execution. Linux additionally kills the task when its supervisor dies.
Local worker shutdown records `WorkerExecutionInterrupted` as retryable before
notifying the child. It follows the execution retry budget instead of being
classified as permanent cancellation.

Lease deadlines are converted to the worker's monotonic clock on receipt, using
the coordinator's HTTP `Date` response header as the reference, so worker
wall-clock skew does not end tasks early. Without that header the worker clock
is used. Storage credential expiry is checked against the worker clock, which
is also the clock used to sign storage requests; keep worker clocks synchronized.

Worker-local faults, such as a socket that cannot be bound, a subprocess or
cgroup that cannot be created, or local journal I/O, are reported as retryable
`WorkerLocalFailure`. An artifact that does not verify against its grant is
retryable `TaskArtifactUnverified`. Only a grant that cannot run as issued is
the permanent `InvalidTaskGrant`. A grant the worker refuses is journaled as a
failure for that attempt, and the worker keeps polling.

## Reporting and restart recovery

Each attempt directory retains `result.pb` or `failure.pb`, followed by `ack.pb`
when the coordinator returns an authoritative disposition. These are immutable,
atomically published, synced protobuf files. An exact local replay is harmless;
a conflicting result cannot overwrite the existing record. Credentials never
enter these files. Completed output files remain under their registered attempt
prefix for publication or coordinator retention cleanup.
Cancellation of a local RPC does not cancel its completion writer or release
its exclusive completion guard. Recovery rejects conflicting completion files.
An ordered pending index is rebuilt once on startup; normal polling and reporting
inspect outstanding records without repeatedly traversing acknowledged history.

### Journal retention

Each acknowledged attempt directory is kept for `journal_retention_hours`
after its `ack.pb` was written (30 days by default), then removed. Removal runs
at startup, before the recovery scan, and hourly while the worker runs, so
both disk use and the startup scan stay bounded by the retention window.

- Unacknowledged records (`result.pb` or `failure.pb` without `ack.pb`) are
  never removed; they are still owed to the coordinator.
- Quarantined records (`quarantine.pb`) are never removed. Inspect them, then
  delete the attempt directory by hand while the worker is stopped.
- A directory is first renamed into the state directory's `.trash/`, the
  rename is synced, and only then is it deleted. A crash mid-removal leaves
  content only in `.trash/`, which the next pass empties and recovery never
  reads.
- Removing an acknowledged record cannot allow a second execution: an
  acknowledgment follows the coordinator's decision on an ended attempt, and
  the coordinator never grants an ended attempt again.

A failed hourly pass is logged (`journal retention pass failed`) and retried
at the next interval; it does not stop execution or reporting. A failure at
startup stops the worker, like any other journal I/O error. To archive history
before it expires, copy attempt directories that contain `ack.pb` out of the
state directory; never move or edit unacknowledged records.

On restart the worker reports unacknowledged records with their original session
and receipt digest before requesting more execution. A timeout retries the same
request, with reporting backoff capped at 30 seconds. Acknowledgments include refusals such as stale attempt, cancellation,
and invalid result; none authorizes rerunning that attempt. When the coordinator
definitively rejects a report (for example `FailedPrecondition` or `NotFound`),
the worker queries `GetAttemptStatus` and acknowledges only a conclusive ended
disposition. A record that is rejected three times in a row without such a
disposition, or that is corrupt, is quarantined: `quarantine.pb` records the
reason (`ReportRejected` or `InvalidJournalRecord`), the record is kept for
inspection, and polling resumes. Authentication failures and transient errors
keep the report pending. Conflicting completion files still fail startup.
Keep the state directory when restarting, and do not edit journal files.

Run `cargo test -p bergsmith-worker -p bergsmith-worker-bin` for supervisor,
private RPC, journal recovery, and real task-subprocess regression tests. The
[distributed recovery/release suite](distributed-acceptance.md) runs with
`make test-distributed` (BS-016).
