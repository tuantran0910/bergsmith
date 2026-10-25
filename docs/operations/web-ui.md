# Web UI

The browser application in `web/` covers manual maintenance over the
[browser API](api.md). The decision is recorded in
[ADR 0037](../adr/0037-web-ui-and-generated-client.md).

## Journeys

| Page | What it does |
|---|---|
| Sign-in | Asks for the organization slug and starts `/auth/login`; returns to the page that asked |
| Catalogs | Lists authorized connections and opens guided setup, with a capability check before saving |
| Catalog connection | Shows settings, capabilities, read-only storage profiles, unresolved publications, and registered names; connection administrators can edit, discover, register, and remove names |
| Tables | Lists the tables the session may see, named `catalog.namespace.table` through their aliases |
| Table | Shows the table's names, previews maintenance (outcome, unsupported features, bins, estimate), and runs it |
| Jobs | Lists jobs with execution and publication status side by side |
| Job | Shows execution and publication separately, streams events live, requests cancellation, and retries with a fresh plan |
| Policies | Lists maintenance policies and creates or edits one, sending the revision it read; a conflict stops the save until the current revision is reloaded |
| Schedules | Lists schedules by the table they maintain, with their policy, cron, time zone, window, service identity, state, and next instant; creating one needs a policy, a table, and an eligible service identity |
| Schedule | Shows the configuration, the next five instants the coordinator computed, and the evaluation history with the job each admitted run created and the reason a run was coalesced or skipped; timing and the enabled flag are editable, the bound policy, table, and identity are not |
| Workers | Current worker sessions with status, running attempt, and capacity; shown only to organization-wide administrators |
| Audit | Organization audit history; shown only to organization-wide administrators |

Connection setup and registration are described in
[Connecting a catalog](catalog-connections.md) and
[ADR 0051](../adr/0051-catalog-connection-pages-and-draft-checks.md).
Maintenance policies and schedules are configured on the Policies and Schedules
pages (BS-024.T1 and BS-024.T2, ADR 0038); both are shown only to
organization-wide administrators, because a table or connection grant does not
administer organization configuration. Creating a schedule also needs
`inventory:read` to choose a table and `workers:manage` to choose a service
identity; the page names the missing grants rather than offer a control that
cannot work.

## Reading job status

| Execution | Meaning |
|---|---|
| Planning, Queued, Running | Work is in progress |
| Outputs ready | Rewritten files exist; this is not success |
| Stopping after cancellation | Cancellation won; the job is winding down |
| Nothing to rewrite | The job ended as `NoOp` |
| Stopped / Failed | The job ended without publishing |

| Publication | Meaning |
|---|---|
| Not started | No commit request is authorized |
| Commit authorized | A commit request may be sent; cancellation is now too late |
| Committing | The commit request is in flight |
| Outcome unknown | The catalog did not confirm the result; Bergsmith resolves it from publication evidence |
| Confirmed | The exact publication is confirmed (`Succeeded`) |
| Not needed / Not published | Nothing was or will be published |

`ReplanRequired` and `CommitOutcomeUnknown` also show a notice with the next
step. A `ReplanRequired` job can be retried, which creates a new job with a
fresh plan. Do not start new maintenance on a table while a job's commit
outcome is unknown.

## Errors and retries

Every API error shows its reason and the operator action. When the API marks a
failure as safe to repeat, the coordinator could not be reached, or a gateway
answered 502, 503, or 504, the page offers "Retry the same request". A
submission whose outcome is unknown keeps its idempotency key, whether it is
retried or run again, so it can never create a second job.

Catalog creation has no idempotency key: an uncertain save asks you to inspect
the catalog list before adding again. A settings conflict or uncertain update
requires explicitly reloading the saved revision. Catalog mutations are never
retried automatically.

Signing out, an expired session, or a sign-in as someone else in another tab
(checked when the tab regains focus) discards everything the page had read.

## Development

The UI is served from the same origin as `/auth` and `/api/v1`. In the Docker
stack, an nginx `web` container serves the build and proxies those paths to the
coordinator (BS-020.T1, [serving](serving.md)). `make app-up` builds the images,
runs migrations and development bootstrap, and serves the UI at
`http://127.0.0.1:8080`.

For UI work, run the Vite development server, which proxies both to a
coordinator:

```sh
BERGSMITH_COORDINATOR_URL=http://127.0.0.1:8080 make web-dev
```

Open `http://127.0.0.1:5173`. The OIDC redirect URI must point at the same
origin (`http://127.0.0.1:5173/auth/callback`).

| Command | Purpose |
|---|---|
| `make web` | Generated-client check, format, lint, typecheck, tests with coverage, build |
| `make web-audit` | `npm audit` of the locked dependencies |
| `npm --prefix web run generate` | Regenerate `web/src/api/schema.d.ts` after changing `openapi.json` |
| `npm --prefix web test` | Run the tests once |
