# Worker sessions and task grants

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
the worker. Heartbeats refresh the worker's storage credentials and return renewal,
cancellation, or fencing instructions.

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
automatic admission, retry timers, and queue reconstruction; BS-015 adds worker
runtime loops and subprocess enforcement. A drained or expired worker is not
proof that its process has stopped writing. Preserve its registered output prefix
under the retention rules in [publication.md](publication.md).

The TLS RPC test suite generates ephemeral certificates with OpenSSL; OpenSSL
must be available in development and CI. No private test key is checked in.
