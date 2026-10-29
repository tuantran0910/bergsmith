# M5 acceptance run (BS-020.T4)

This is the recorded result of the M5 acceptance pass over the correctness and
failure matrix in the project plan §5. It is evidence from one environment, not
a certification of every row in the [support matrix](../../support-matrix.md).
Rows move to **Certified** only when their own acceptance tests pass.

Run date: 2026-10-29, repository `main` at `6fe0f22` (after BS-032.T8).

## Environments

| Run | Environment |
|---|---|
| Workspace and catalog/storage contract suite | macOS 26 arm64 (Darwin 25.3.0), Docker through OrbStack; Compose PostgreSQL 17.6-alpine with TLS, Lakekeeper v0.14.1, RustFS 1.0.3, S3 vended credentials |
| Linux process acceptance | OrbStack Ubuntu 25.10 arm64, kernel 6.17.8, systemd 257, delegated cgroup v2 (`cpuset cpu io memory pids`), PostgreSQL 17.10 |
| Packaged stack | `make app-up` on the same macOS host; coordinator 0.1.0, web image, PostgreSQL, RustFS, Lakekeeper, Keycloak |

## Results

### Workspace and live catalog/storage suite

`make test` against the Compose services passed **1431 tests, 0 skipped** in
214.186 s. This covers the §5 native-fixture, delete, planning, publication,
assignment, result, cancellation, retry, authorization, and scheduling rows with
real Iceberg metadata and Parquet files. The catalog path used Lakekeeper
v0.14.1 with table-scoped credentials vended from RustFS; it did not use AWS S3.

### Linux process acceptance

`deploy/docker/distributed-scope.sh -o services-up test-distributed` ran inside
a delegated systemd service with `Delegate=yes`, `BERGSMITH_TEST_CGROUP_ROOT`
pointing at the delegated cgroup, and a local PostgreSQL. Result:

```text
running 5 tests
test coordinator_process ... ok
test distributed::configured_cgroup_paths_round_trip_as_worker_paths ... ok
test pending_cancellation_after_intent_survives_process_restart_without_publication ... ok
test lost_grants_worker_death_and_lost_receipts_survive_coordinator_restart ... ok
test one_hundred_groups_across_three_workers_publish_once_after_process_failures ... ok

test result: ok. 5 passed; 0 failed; 0 ignored
```

This is the first local Linux run with real cgroup attachment: three worker
supervisors, native task subprocesses, lost grants and receipts, worker death,
coordinator restart, and one publication. It does not exercise memory or CPU
pressure.

### Packaged Compose stack

`make app-up` brought every container to healthy and the nginx origin answered:

| Request | Result |
|---|---|
| `GET /healthz` | 200 |
| `GET /readyz` | 200 |
| `GET /` | 200, with the coordinator CSP, `nosniff`, and `X-Frame-Options: DENY` |
| `GET /api/v1/catalogs` unauthenticated | 401 |
| `GET /api/v1/workers` unauthenticated | 401 |

The stack is development-only by design (fake credentials, `fsync=off`,
plain-HTTP OIDC, non-Secure cookies). The worker profile was not started because
it needs delegated Linux cgroup v2. No sign-in, catalog connection, or job was
executed against this stack.

## Gates not satisfied

| Gate | Why it is open | What it needs |
|---|---|---|
| Linux CPU and memory pressure enforcement | No end-to-end task was run under a constrained cgroup. Unit tests cover `exit_reason` and OOM-event parsing, not a real task killed by `memory.max` or throttled by `cpu.max` | A Linux host with delegated cgroups plus a fixture or fault-injection hook that makes a real task exceed its task cgroup |
| Disk exhaustion | No test forces `ENOSPC` during a rewrite, output write, or journal append | A small filesystem or fault-injecting object store, and a classification assertion |
| Slow storage | No test raises I/O latency past `io_timeout_ms` during execution | Latency injection (FUSE, `tc`, or a proxy) and a classification assertion |
| Declared production catalog/storage | Only Lakekeeper v0.14.1 and RustFS 1.0.3 ran; the matrix target is Lakekeeper v0.14.0 and Amazon S3 | Credentials and endpoints for the production combinations, or a decision to recertify against the shipped versions |
| Compose as a production deployment | The stack is documented as development-only; Kubernetes/Helm is the M8 production path | M8 packaging, not this gate |
| Worker profile in the packaged stack | Needs delegated cgroup v2, absent on macOS | Run the stack on Linux with the worker profile and execute a job |

The project's own rule applies: process recovery certification does not
substitute for pressure, disk, latency, or production catalog/storage checks.

## Conclusion

The M5 gate is **not passed**. The run adds two pieces of evidence: the live
catalog/storage suite on the Compose versions, and a local Linux cgroup process
acceptance. No support-matrix row was moved to Certified. The first usable
release still waits on the four production gates above.

## Reproduce

```sh
make services-up
make test
make services-down

# On Linux with delegated cgroup v2 and a reachable PostgreSQL:
sudo systemd-run --quiet --wait --pipe --collect --property=Delegate=yes \
  --uid="$(id -u)" --gid="$(id -g)" --working-directory="$PWD" \
  --setenv="PATH=$PATH" --setenv="HOME=$HOME" \
  --setenv="DATABASE_URL=postgres://bergsmith:bergsmith@127.0.0.1:5432/bergsmith" \
  /bin/bash deploy/docker/distributed-scope.sh -o services-up test-distributed

make app-up    # development stack only; `make app-down` afterwards
```
