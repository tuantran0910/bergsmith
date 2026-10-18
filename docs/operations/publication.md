# Publication and recovery

Publication makes a completed rewrite visible by committing a replacement
Iceberg snapshot through the REST catalog. Workers write Parquet files; only
the coordinator may publish them. Removing source files from a new snapshot
does not delete the underlying objects or older snapshots.

The coordinator's BS-012 library flow is:

1. Persist a grant, then call `results::register_attempt_outputs` before starting
   its worker. The prefix must be empty and derived from the persisted plan.
2. Once output writes are closed, call `results::accept_result`. It verifies
   object metadata and footers, then records the receipt and inventory atomically.
   The last accepted receipt moves the job to `ReadyToCommit`.
3. Call `durable_publication::prepare_publication` with the original baseline and
   catalog connection identity. This writes immutable intent and table barrier
   together with `Committing`. No commit has been authorized yet.
4. Call `durable_publication::publish`. It restores the exact request, rechecks
   the original baseline, durably authorizes the first send, sends once, and
   records the outcome. Database transactions never enclose storage/catalog I/O.

Receipt and leadership lease checks use current PostgreSQL time after acquiring
their serialization locks, so waiting for a lock cannot extend a lease. Publication
locks the job before checking leadership. Partition values use the newest retained
schema that binds their spec, matching the planner after type promotions.

On restart, use `publications::recoverable_jobs` and the persisted connection
returned by `durable_publication::publication_connection`. An intent without send
authorization can still be published or cancelled. For an authorized job, call
`durable_publication::reconcile`; do not call the catalog mutation API again.
The recovery list also includes intents left in `Cancelling`; reconciliation
finishes their pre-authorization cancellation and releases the barrier.
Automatic startup scheduling and worker RPC orchestration are later tasks.

| Observation | Durable outcome |
|---|---|
| Cancellation before send authorization | Publication is prevented; finish cancellation and release its barrier |
| Conclusive first-send success | `Succeeded`; release barrier |
| Conclusive first-send rejection | `ReplanRequired` or `Failed`, or pending cancellation; release barrier |
| First sender proves it never sent | `Failed`, or pending cancellation; release barrier |
| Crash after authorization, timeout, or uncertain response | Reconcile without another send; retain barrier and objects |
| Matching retained snapshot, even behind current head | `Succeeded`; release barrier |
| Missing or mismatched evidence | `CommitOutcomeUnknown`; retain barrier and objects |
| Catalog unavailable | Leave durable state and retention intact; retry read-only reconciliation later |

Use `publications::output_inventory` to inspect registered prefixes and finalized
accepted objects. A prefix can contain failed or incomplete writes even when no
receipt was accepted. Multipart-upload enumeration is not exposed by the current
storage adapter. S3 requires the existing `AbortIncompleteMultipartUpload` bucket
lifecycle rule to cover crashed uploads; local and memory storage do not use
multipart uploads. Inventory does not delete anything, including abandoned output.

Keep unresolved outputs and relevant snapshot/manifest history readable. Exclude
the protected attempt prefixes from external orphan cleanup or disable that
cleanup while outcomes remain unresolved. Missing history, a different current
head, elapsed time, or a rejection from a later request cannot prove the original
authorized request will never publish. Investigate these cases without manually
forcing a terminal state or clearing the table barrier.

After a database restore, reconcile potentially forgotten intents against retained
metadata and backups before enabling writes. This library does not automatically
recover intents lost from both the database and available evidence.
