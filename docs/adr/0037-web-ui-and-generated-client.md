# 0037. Web UI and generated API client

- Status: Accepted
- Date: 2026-10-21
- Plan reference: BS-018.T4, public API and UI

## Context

M4 requires people to complete manual maintenance in a browser: browse
inventory, preview a table, run maintenance, follow its progress, cancel or
retry it, and read audit history. The plan fixes the stack (React,
TypeScript, Vite, TanStack Query) and requires the client to be generated from
the OpenAPI contract. It also requires the UI to show execution completion
separately from publication completion, and to present `ReplanRequired` and
`CommitOutcomeUnknown` distinctly from ordinary failures.

Three constraints shape the work:

- Several UI areas in the plan have no API yet: connection setup and other
  configuration writes, schedules and policies (BS-019), and worker health.
- The coordinator binary does not start an HTTP server; `/auth` and `/api/v1`
  are library routers (ADR 0034).
- The repository had no JavaScript toolchain, and `cargo deny` does not see
  npm dependencies.

## Decision

### Scope

- The UI covers what the API serves: sign-in by organization slug, sign-out,
  catalog and table browsing, previews (including typed rejections as the
  table's capability status), submission, the job list, a job page with live
  events, cancellation with its reported outcome, fresh-plan retry, and
  organization audit history.
- Areas without an API are not shown, not stubbed. They join the UI with the
  task that adds their API.

### Contract and client

- `openapi-typescript` generates `web/src/api/schema.d.ts` from
  `crates/coordinator/src/api/openapi.json`, and `openapi-fetch` is the typed
  runtime. The generated file is committed; `npm run check-generated` fails
  when it differs from a fresh generation.
- `/auth` belongs to the identity router, not the `/api/v1` contract. Its two
  calls (`GET /auth/session`, `POST /auth/logout`) are typed by hand in one
  module; sign-in is a full-page navigation to `/auth/login`.
- Values that can exceed 2^53 travel as decimal strings. Audit event IDs
  already did; the preview's `baseline.snapshot_id` changes from an integer to
  a string in this task, because browsers parse JSON numbers as doubles and
  Iceberg snapshot IDs use all 64 bits.

### Behavior

- Execution and publication are two separate status lines. Publication is
  "Confirmed" only for `Succeeded`; `ReadyToCommit` means outputs are ready,
  and `NoOp` reports that publication was not needed. `ReplanRequired` and
  `CommitOutcomeUnknown` show a notice with the operator action.
- Errors show the API's `reason` and `operator_action`. A `RetrySameRequest`
  failure, a request that never reached the coordinator, or an untyped 502,
  503, or 504 from a gateway offers "Retry the same request". Queries retry
  such failures twice automatically; mutations never retry on their own.
- Submission and retry generate one `Idempotency-Key` per user action and keep
  it while the outcome is uncertain: a retryable failure, or any 5xx without a
  typed body, since a gateway can time out after the coordinator committed.
  Retrying, or running the action again, then replays the original operation
  instead of creating a second job. Success or a typed permanent failure ends
  the action.
- Mutations echo the `bergsmith_csrf` cookie in `x-csrf-token`.
- Cached data belongs to one identity. A 401 from any request, sign-out, or a
  session check that returns another organization or principal (re-checked
  when the tab regains focus) cancels and removes every cached read and
  mutation result, and the application shell is re-created so no page keeps
  the previous identity's state.
- Every query passes TanStack Query's abort signal to `fetch`, so a superseded
  or abandoned read stops its request.
- The job page opens one `EventSource`. The browser resumes with
  `Last-Event-ID`; events are deduplicated by sequence. Events refresh the job
  resource through a coalescing refresher: at most one read runs, and requests
  during it collapse into one follow-up read that starts after the newest
  event, so replaying a long history costs a few reads, not one per event.
  The client closes the
  source after a terminal state's event, and also when the browser starts
  reconnecting to a job already known to be terminal, because the server ends
  a terminal stream immediately and an open `EventSource` would reconnect
  forever. If the browser gives up, the page offers to reconnect with
  `?after=` the last sequence received.

### Toolchain and gates

- `web/` holds the application: React 19, TypeScript 5.9, Vite 8, TanStack
  Query 5, Vitest 5 with Testing Library and jsdom, ESLint with the strict
  type-checked typescript-eslint rules and the React hooks rules, and
  Prettier. TypeScript stays on 5.9 because `openapi-typescript` and
  typescript-eslint do not support 6 or later yet.
- Node 24 LTS (`web/.nvmrc`), npm with a committed `package-lock.json`, exact
  dependency versions, and `npm ci`. Routing is a small in-application path
  router rather than another dependency.
- `make web` runs, in order: generated-client freshness, formatting, lint,
  typecheck, tests with at least 80% line coverage, and the production build.
  `make web-audit` runs `npm audit` for production dependencies at any
  severity and all dependencies at high severity. Both are part of `make ci`
  and run as the "Web UI" CI job.
- Changes under `web/` select no Rust package in `make test-changed`.

### Serving

- This task does not wire the coordinator's HTTP runtime; BS-020.T1 adds a
  `serve` command that mounts `/auth`, `/api/v1`, and the built assets on one
  origin. The session cookie is `SameSite=Lax` and the CSRF cookie must be
  readable by the page, so the UI and the API must share an origin.
- For development, `make web-dev` serves the UI on `127.0.0.1:5173` and
  proxies `/api` and `/auth` to `BERGSMITH_COORDINATOR_URL` (default
  `http://127.0.0.1:8080`).

### Acceptance

- Journey tests run the whole application against a fake coordinator whose
  responses are typed by the generated schema, so a contract change breaks
  them at compile time, and a controllable fake `EventSource`.
- A browser run against the Compose stack needs the `serve` runtime and is
  part of the BS-020.T4 acceptance run.

## Consequences

- Contributors need Node 24 and npm in addition to the Rust tools; `make ci`
  now installs the npm dependencies.
- A change to `openapi.json` must regenerate the client in the same change, or
  the web gate fails.
- Changing `baseline.snapshot_id` to a string is a breaking change to the
  `/api/v1` preview response. No released client depends on it; the API is
  not yet served by any released binary.
- npm license policy is not enforced by a gate yet; dependency additions are
  reviewed by hand, as `AGENTS.md` requires for Rust crates.
- M4 is not certified by this task: the UI can be exercised end to end only
  once the coordinator serves it (BS-020.T1).
