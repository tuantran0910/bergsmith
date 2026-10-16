# 0008. Local gRPC between worker supervisor and task subprocess

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §2 Supervisor–subprocess contract

## Context

Native tasks run in a subprocess for isolation. Vended credentials expire, so refreshed credentials must reach a running task.

## Decision

- The supervisor serves `bergsmith.worker.local.v1` on a Unix domain socket in a per-attempt `0700` directory; the subprocess is the client.
- RPCs: `GetTask`, `ReportProgress`, `WatchControl` (cancellation and refreshed credentials), `ReportResult`, `ReportFailure`.
- Cancellation escalates from `WatchControl` to `SIGTERM` to `SIGKILL`. The subprocess runs in its own process group and, on Linux, dies with its supervisor.

## Consequences

- An exit without a result or failure report is a crash, mapped to `RetryTask`.
- Supervisor and subprocess are always the same build, so the local protocol versions with the binary.
