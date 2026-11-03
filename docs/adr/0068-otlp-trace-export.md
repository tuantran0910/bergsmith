# 0068. OTLP trace export

- Status: Proposed
- Date: 2026-11-03
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-030.T3; ADR 0044
  (observability baseline)

## Context

ADR 0044 made `tracing` spans the trace baseline: planning and publication
run each job in a span naming `job_id`, and every worker RPC runs in a
`worker_rpc` span. Those spans reached only the log lines, so following one
attempt from a worker to the coordinator meant joining two processes' logs by
hand. ADR 0044 deferred an exporter to production work.

An exporter adds an outbound connection, a background queue, and a new
consumer of span data. The constraints that shaped the decision were:

- export must be off unless an operator configures it, and cost nothing
  then;
- telemetry must never block or fail a request, a job, or shutdown;
- span data leaves the deployment, so it must not carry credentials, row
  data, or SQL text, and a client must not be able to steer sampling;
- configuration should be the standard OpenTelemetry variables operators
  already use.

## Decision

- **OTLP over gRPC, opt-in by environment.** The coordinator and the worker
  supervisor export spans when `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or
  `OTEL_EXPORTER_OTLP_ENDPOINT` is set, unless `OTEL_SDK_DISABLED=true` or
  `OTEL_TRACES_EXPORTER=none`. The exporter uses the workspace's tonic and
  aws-lc TLS stack with the system roots, plus a CA bundle from
  `OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE` or `OTEL_EXPORTER_OTLP_CERTIFICATE`.
  The standard variables configure headers, timeout, compression (`gzip` or
  `none`), and batch sizes. Only `grpc` is accepted as the protocol. The
  endpoint is parsed before the exporter sees it, and an unsupported value of
  any variable Bergsmith checks stops the process at startup, as
  `BERGSMITH_LOG_FORMAT` already does. Error messages name the variable that
  was read, never its value, and the exporter's own build errors are not
  repeated, because they can contain the endpoint.
- **One telemetry crate.** `bergsmith-telemetry` installs the log
  subscriber, the exporter, and the sampler for both binaries, replacing the
  copy of the logging set-up each binary had. It also provides the W3C
  propagation helpers, and behind a `testing` feature, an in-memory span
  capture for other crates' tests.
- **Only Bergsmith's spans leave the process.** The export filter admits
  only targets whose crate is `bergsmith` or `bergsmith_*`, at the levels in
  `BERGSMITH_TRACE_FILTER` (default `info`). Dependency spans and events,
  including SQL statements, HTTP clients, and the exporter's own requests,
  are never exported, whatever the directives say. Span fields stay IDs,
  states, method names, and route templates.
- **Worker trace context reaches the coordinator; nothing else crosses a
  boundary.** Each worker RPC carries `traceparent` and `tracestate` from the
  current span. The coordinator's worker RPC layer keeps that context in the
  request's extensions, and the worker service adopts it as the RPC span's
  parent only after the worker's token authenticates. Spans start lazily
  (entering one does not start it), so the span has no trace ID or sampling
  decision until then. An unauthenticated call therefore starts a new trace:
  it can neither choose its trace ID nor force sampling. A granted attempt runs in a `task_attempt` span naming its job,
  task, and attempt. A report sent later from the journal runs in its own
  `report_attempt` span with the attempt ID, because it may be sent by a
  later process. Coordinator HTTP requests under `/api/v1` and `/auth` run
  in an `http_request` span named by method and route template, and always
  start a new trace: a browser's `traceparent` is ignored. Probes and static
  files are not traced. Bergsmith never sends trace context to a catalog,
  object store, or notification destination.
- **A remote parent cannot raise the sample rate.** `OTEL_TRACES_SAMPLER`
  accepts `always_on`, `always_off`, `traceidratio`, and their `parentbased_`
  forms. The default is `parentbased_always_on`. In the parent-based forms, a
  local parent's decision is followed and an unsampled parent drops its
  children. A remote sampled parent, however, is decided again by the root
  sampler, as a second bound behind authentication. Trace-ID ratio sampling
  is a function of the trace ID alone, so a worker and a coordinator
  configured with the same ratio make the same decision for a shared trace.
- **Bounded cost.** Spans are queued in the SDK's batch processor, which
  exports on its own thread from a bounded queue and drops spans when the
  queue is full. Creating a span never waits for the collector. Shutdown
  waits at most five seconds for queued spans. An export failure is logged
  once when an outage starts and once when export recovers, without the
  request's headers.
- **Task subprocesses do not export.** They start with a cleared environment
  (worker invariant). The supervisor's `task_attempt` span covers their
  lifetime, and their log lines continue to name the attempt.

## Consequences

- An operator can follow one attempt across processes. The worker's
  `report_attempt` span parents the coordinator's span for the report it
  sends, and `task_attempt` parents the report of a refused grant. The
  attempt's spans share its `attempt_id`, and the job's planning and
  publication spans share its `job_id`.
- With no endpoint configured there is no exporter, no background thread,
  and no outbound connection. Log lines do change: events inside a worker RPC
  show the new span name and `otel.*` and `rpc.*` fields, and API requests
  gain an `http_request` span with the route template. A client cannot mint
  span names, because methods outside the standard set are named `_OTHER`.
- New dependencies: `opentelemetry`, `opentelemetry_sdk`,
  `opentelemetry-otlp` (gRPC, aws-lc TLS), and `tracing-opentelemetry`, all
  at 0.33/0.34. `opentelemetry-proto` is a test dependency for the
  in-process collector.
- The parent-based samplers depart from the specification for remote
  parents, as described above. A deployment that wants every worker trace
  continued on the coordinator uses `parentbased_always_on`, the default.
- Not done: trace context in the job ledger, which would join planning,
  attempts, and publication into one trace; spans inside task subprocesses;
  OTLP metrics or logs; and OTLP over HTTP. Each needs its own decision.
