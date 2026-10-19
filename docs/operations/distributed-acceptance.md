# Distributed recovery acceptance

Run `make test-distributed` to build the worker and exercise BS-016 against the
Compose PostgreSQL service. OpenSSL and the OS-provided `mkfifo` must be
available. Linux also requires
`BERGSMITH_TEST_CGROUP_ROOT` pointing to a writable cgroup v2 subtree with the
CPU, memory, and PID controllers enabled for children. The CI workflow supplies
that delegation through a systemd service with `Delegate=yes`, running as the
runner user. Its startup script moves supervisors into a leaf before enabling
child controllers. Task attachment stays inside the delegated service, including
the common ancestor permission required by cgroup v2. For a similar local
systemd setup, run the script with `sudo systemd-run --wait --pipe --collect
--property=Delegate=yes --uid="$(id -u)" --gid="$(id -g)"
--working-directory="$PWD" --setenv="PATH=$PATH" --setenv="HOME=$HOME"
/bin/bash deploy/docker/distributed-scope.sh`.
macOS runs exercise the development resource path described in
[workers.md](workers.md).

The suite starts isolated coordinator processes serving the production worker
service and scheduler over TLS, and actual `bergsmith-worker run` processes.
Workers execute native Iceberg/Parquet rewrites through their private task
subprocesses. Coordinator startup rebuilds dispatch and recovers publications
from PostgreSQL; no in-memory queue is transferred across restarts. Fixtures use
local storage and a controlled Iceberg REST endpoint that applies the original
commit requirements and metadata updates. This does not certify a production
catalog or remote object store.

The small recovery test and the 100-group release test both:

1. Lose a response after persisting a grant and verify the same poll replays it.
2. Deliver the replayed grant and hold the real native task's pinned-metadata
   read on a FIFO. Confirm its subprocess exists, inject an incomplete output
   under the attempt prefix, kill the supervisor, and verify subprocess exit.
   Linux must terminate the child while the read remains blocked; on macOS the
   test releases the blocked read after supervisor death so control-stream loss
   can be observed. Restore the original metadata, expire only the abandoned
   lease, and restart the coordinator.
3. Hold dispatch until three distinct worker processes each have a durable grant.
4. Lose an accepted receipt's response, restart the coordinator, and verify the
   immutable report receives `AlreadyAccepted` without replacement execution.
5. Require one accepted receipt per task, participation by all three workers,
   and exactly one additional attempt for the abandoned grant.
6. Send the commit from the coordinator child. Apply the original requirements
   and updates in the controlled catalog, then hold its response. Kill the
   publishing coordinator and require the ledger still to contain an authorized
   `Committing` job with no outcome recorded. Release the response only after
   process death, then start a new coordinator to recover retained evidence.
7. Require immutable publication identity and request bytes, one catalog POST,
   one new snapshot, and equality of the independent before/after live-row
   multisets, including duplicates and nulls.

A separate process test records cancellation after creating an intent but before
send authorization. Startup finishes cancellation, releases the publication
barrier, preserves terminal command replay, and sends no catalog commit.
It then creates a linked retry with a fresh native plan and task artifact
identities, requiring every task to be pending with no inherited receipts while
the cancelled job stays unchanged.

Fault checkpoints are atomic files announced through Unix datagrams. Execution
barriers await sticky watch notifications, and task completion is signalled
after the final receipt is accepted. Bounded observations check process health,
FIFO-reader readiness, and process exit without busy loops or repeated ledger
queries. The token file is created once and remains read-only across launches;
cgroup paths round-trip as JSON strings. Lease advancement is test-only and
follows a
confirmed process death. Child processes are killed on teardown; a failed run
retains its temporary directory and includes its location in the assertion so
coordinator and worker logs can be inspected.

`make ci` includes this suite. CI runs coverage and process acceptance in one
delegated Make invocation so their shared Compose services start once. The
systemd launch preserves compiler flags and incremental-build settings from
toolchain setup so cached dependencies remain reusable. The delegation script
defaults to `test-distributed` and accepts other Make targets and variable
assignments as arguments. The `distributed-tests` feature keeps tests that
need a prebuilt worker and host delegation separate from ordinary coverage runs;
workspace check and clippy still compile them. Existing PostgreSQL, TLS/native
receipt, scheduler, publication, and subprocess suites remain part of the M3
gate. They cover cancellation/authorization races, receipt/expiry fencing,
three-attempt limits and persisted retry timers, permanent failures, baseline
conflicts, fresh-plan identities, unknown-outcome barriers, and local reporting
recovery. See [scheduling.md](scheduling.md) and [publication.md](publication.md).

A distributed run with normal memory use does not establish kernel enforcement
under memory pressure or CPU saturation. Delegated-host resource certification
must be reported separately from process recovery acceptance.
