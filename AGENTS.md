# AGENTS.md

Rules for everyone who changes this repository, human or coding agent. When a
rule here conflicts with a personal habit or a tool default, this file wins.
When it conflicts with `.private/BERGSMITH_PROJECT_PLAN.md`, the plan wins; fix this
file in the same change.

## 1. Project

Bergsmith is a self-hosted Iceberg maintenance platform written in Rust. A
coordinator plans and publishes; pull-based workers rewrite data; PostgreSQL
holds the durable execution ledger; standard Iceberg REST catalogs keep table
metadata authority.

- `.private/BERGSMITH_PROJECT_PLAN.md` is the authoritative specification: scope,
  contracts, the backlog (§4), and the task breakdown (`BS-NNN.Tn`). It is a
  private, maintainer-local file and is not tracked by git (`/.private/` is
  ignored). Never commit it or copy its contents into tracked files wholesale.
  Without it, the ADRs are the public record of each decision.
- `docs/adr/` records each architecture decision. `docs/support-matrix.md`
  lists what is supported and certified.
- Read the plan section for the area you touch before changing it.

## 2. Repository layout

| Path | Contents |
|---|---|
| `crates/common` | Typed IDs, lifecycle states and transition rules, failure dispositions. No I/O. |
| `crates/protocol` | Protobuf contracts and generated tonic code |
| `crates/catalog` | Iceberg REST integration, vended credentials, commit adapter |
| `crates/storage` | Backend-neutral object storage: locations, profiles, `ObjectStore` |
| `crates/maintenance` | Planner, rewrite executor, delete application |
| `crates/fixtures` | Dev-only: native Iceberg fixtures and the live-row oracle (ADR 0016) |
| `crates/persistence` | SQLx repositories and migrations |
| `crates/coordinator` | Coordinator services |
| `crates/worker` | Worker supervisor and task subprocess |
| `bin/coordinator`, `bin/worker` | Thin binaries: startup wiring only |
| `web/` | Browser UI (React, TypeScript, Vite) and its client generated from the OpenAPI contract |
| `scripts/` | Developer tooling, such as changed-area and affected-package selection |
| `deploy/` | Container, Compose, and Kubernetes assets (see below) |
| `docs/` | ADRs, support matrix, operator documentation |

Dependency direction is enforced by `deny.toml`:

- Only `bergsmith-coordinator` may depend on `bergsmith-persistence` or
  `bergsmith-catalog`.
- `bergsmith-common` depends on no other workspace crate.
- Binaries hold no logic that a library crate could test.
- Protobuf changes follow the compatibility rules in
  `crates/protocol/README.md`.

### Deployment assets

Everything used to build, run, or deploy Bergsmith outside `cargo` lives under
`deploy/`. Nothing deployment-related goes in the repository root.

| Path | Contents |
|---|---|
| `deploy/docker/` | Dockerfiles, image build context, and Docker Compose files for development and tests |
| `deploy/k8s/` | Kubernetes manifests and the Helm chart (milestone M8) |

- Pin every image by an exact version tag (or digest), never `latest`.
- Never commit secrets. Development credentials are allowed only in the
  `deploy/docker/` Compose files and must be obviously fake. Mark
  development-only settings (such as `fsync=off`) with a comment.
- Wrap Compose and image commands in `Makefile` targets so paths are not
  repeated in scripts and CI.
- Kubernetes assets keep coordinator migrations a separate job
  (`bergsmith-coordinator migrate`); replicas never migrate at startup.

## 3. Commands

| Command | Purpose |
|---|---|
| `make fmt` | Format all code |
| `make check` | Type-check every target |
| `make clippy` | Lint with warnings as errors |
| `make test` | Run tests with cargo-nextest |
| `make test-changed` | Run tests of the packages affected since `BASE` (default `origin/main`); local only |
| `make test-distributed` | Run process recovery and the 100-group, three-worker release test |
| `make doc` | Build docs with warnings as errors |
| `make coverage` | Enforce at least 80% line coverage (binaries excluded) |
| `make coverage-distributed` | Cover workspace and process acceptance together (same delegation as `test-distributed`) |
| `make coverage-html` | HTML report in `target/llvm-cov/html` |
| `make deny` | Advisories, licenses, bans, sources |
| `make machete` | Unused dependencies |
| `make services-up` / `make services-down` | Start or discard PostgreSQL, RustFS, and Lakekeeper (Docker Compose) |
| `make proto-lint` / `make proto-breaking` | buf lint and breaking-change checks (Docker) |
| `make web` | Web UI gates: generated-client freshness, format, lint, typecheck, tests with 80% line coverage, build within the JavaScript size budget |
| `make web-e2e` | Browser gate: the built UI's journeys in Playwright's Chromium under the coordinator's CSP, with axe (WCAG 2.2 AA); host browser |
| `make web-e2e-image` | The same gate in the pinned Playwright image, which CI runs so visual baselines compare against the environment that rendered them |
| `make web-audit` | `npm audit`: production dependencies at any severity, all at high |
| `make observability` | Check and behaviorally test the Prometheus alerts, then execute and render the Grafana dashboards |
| `make ci` | Every gate above, in CI order (coverage runs the tests, and clippy type-checks every target) |
| `make web-dev` | Serve the UI, proxying `/api` and `/auth` to `BERGSMITH_COORDINATOR_URL` |

Required tools: rustup (the toolchain comes from `rust-toolchain.toml`),
`cargo-nextest` (the version required by `.config/nextest.toml`, which CI
installs exactly), `cargo-llvm-cov`, `cargo-deny`, `cargo-machete`, Docker, and
Node.js 24 LTS with npm (`web/.nvmrc`) for the web UI.

Integration tests use the Compose services in `deploy/docker/`, which `make
test` and `make coverage` start first. They read `DATABASE_URL`,
`BERGSMITH_TEST_S3_ENDPOINT`, `BERGSMITH_TEST_CATALOG_URL`, and
`BERGSMITH_TEST_DATABASE_CA` (the CA that signs the test PostgreSQL's
certificate, generated by `make services-up` with `openssl`); override them
to use your own servers. The PostgreSQL role must be allowed to create
databases, because `#[sqlx::test]` gives each test a fresh one.

Distributed process tests also build the worker binary and require OpenSSL and
the operating system's POSIX `mkfifo` utility.
On Linux, set `BERGSMITH_TEST_CGROUP_ROOT` to a writable cgroup v2 subtree with
memory, CPU, and PID controllers enabled for child cgroups; see
`docs/operations/distributed-acceptance.md`. `make ci` includes this gate.

### Local development loop

Run the narrowest check that answers the question, and widen only when it
passes. GitHub Actions selects the affected areas when a pull request is
opened or updated and runs every gate on `main`. `scripts/changed-areas.sh`
is the selector: Rust changes run the Rust gates, web changes run the web
gates, and changes to shared inputs or the workflow run everything. It fails
closed on an unknown path, and `scripts/changed-areas.test.sh` checks the
mapping. When branch protection is enabled, require the `CI` gate rather than
the individual jobs; the gate also fails if the selector itself fails. Do not
run `make ci`, or reproduce its full sequence of gates locally, unless the
user explicitly requests a local full CI run. Opening a PR or asking to wait
for CI and merge does not imply that request; run focused local checks and
delegate the selected gates to Actions.

| When | Run |
|---|---|
| While editing | `cargo check -p <crate> --all-targets` |
| After a logical change | `cargo nextest run -p <crate>` (add `--lib` for unit tests only) |
| Before each commit | Clippy and tests for affected Rust packages, or the relevant web checks |
| Before opening or updating a pull request | Focused local checks for the change; GitHub Actions runs the affected areas |
| Before merging a pull request | All GitHub Actions checks pass on the latest PR head (§4) |

For UI work, use `npm --prefix web run typecheck` and
`npm --prefix web run test -- src/<affected>.test.tsx` during editing. Once
dependencies are installed, a focused browser rerun is
`npm --prefix web run build` followed by
`npm --prefix web run e2e -- <affected>.spec.ts`. Rebuild after every UI source
change: the browser gate serves `web/dist/`, not the Vite development server.
Use `make web` and `make web-e2e` for local behavior checks;
`make web-e2e-image` runs the complete browser gate exactly as Actions does,
including visual baselines. Actions runs `make web` and `make web-e2e-image`
for web changes. Do not repeat unchanged checks without a new failure or
unresolved concern.

- Use cargo-nextest, never `cargo test --workspace`. Nextest reports every
  failure in one run (`fail-fast = false`), so fix them all before re-running.
- `make test-changed` runs every package a change touches plus all packages
  that depend on them, transitively. Changes to shared inputs (root manifests,
  `Cargo.lock`, the toolchain, `.cargo/`, `.config/`, `.github/`, `deploy/`,
  `scripts/`, the `Makefile`, lint and dependency policy) select every package.
  A documentation-only change runs nothing, and so does a change under
  `web/`; run `make web` for those.
- Preview a selection before paying for it:
  `cargo nextest list -E 'rdeps(=bergsmith-storage)'`.
- Never change `RUSTFLAGS`, `RUSTC_WRAPPER`, or the linker for the shared
  `target/` directory; each change rebuilds every crate. Keep local linker
  or cache experiments in an untracked `~/.cargo/config.toml`.
- Debug info stays at Cargo's default in the repository, so debuggers can
  inspect every crate. Reduced debug info shrank a clean `target/debug` by
  about a third without speeding up builds; to trade debuggability for disk,
  opt in from `~/.cargo/config.toml`:

  ```toml
  [profile.dev]
  debug = "line-tables-only"
  [profile.dev.package."*"]
  debug = false
  ```
- `make coverage` and `make ci` build an instrumented copy under
  `target/llvm-cov-target`; they never speed up or replace the inner loop.
- To reclaim disk, use `cargo clean -p <crate>` or delete
  `target/llvm-cov-target`. A full `cargo clean` is not a fix for a
  confusing build error.

## 4. Definition of done

A change is done only when all of these hold:

1. Focused local checks appropriate to the change pass, and all GitHub Actions
   checks pass on the latest pull-request head before merge. Actions owns the
   selected CI gates; a local `make ci` run requires an explicit user request.
2. New or changed behavior has tests, and every bug fix has a regression test.
3. Public items have doc comments explaining behavior and errors.
4. The plan, an ADR, or the support matrix is updated when behavior, a
   contract, or a decision changes.
5. The change maps to one backlog task (`BS-NNN.Tn`) or is clearly
   housekeeping.

Never weaken a gate to make a change pass: no lowering the coverage threshold,
no `#[allow]` without a written reason, no skipped or ignored tests without a
tracking task.

## 5. Rust conventions

### Language and style

- Rust 2024 edition on the pinned toolchain (1.94.1). Format with rustfmt;
  never hand-format against it.
- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/):
  `snake_case` functions and modules, `UpperCamelCase` types, `SCREAMING_SNAKE_CASE`
  constants, conversion names `as_`/`to_`/`into_`, constructors `new` or
  descriptive names such as `generate`.
- Lints live in `[workspace.lints]`; every crate sets `[lints] workspace = true`.
  `unsafe_code` is forbidden. A local `#[allow(...)]` needs a comment saying why,
  and should prefer `#[expect(...)]` so stale allowances fail.
- Keep functions small and modules focused. Prefer `pub(crate)` over `pub`;
  expose only what other crates need.
- Use `#[must_use]` on pure functions and on types whose values must be handled.
- Make invalid states unrepresentable: newtypes for IDs, enums for states,
  validated constructors instead of public fields that can disagree.
- Any Python tooling file (none ships today) is formatted with `ruff format`
  and linted with `ruff check`; keep it out of the shipped runtime and test
  paths.

### Errors

- Library crates define typed errors with `thiserror`. No `anyhow`, no
  `Box<dyn Error>` in library APIs.
- No `unwrap`, `expect`, `panic!`, `todo!`, or `unimplemented!` outside tests;
  clippy denies them. Return errors instead.
- Errors that cross a service boundary carry a stable typed reason and a
  `FailureDisposition` (see the plan, §2 "Failure and retry dispositions").
- Error messages are lowercase, without trailing punctuation, and say what went
  wrong and for which entity.

### Logging, secrets, and output

- Use `tracing` with structured fields. No `println!`, `eprintln!`, or `dbg!`
  outside tests.
- Credentials and tokens are wrapped in a redacting type. They are never
  logged, persisted to PostgreSQL, written to task artifacts, or included in
  errors.

### Determinism

- Anything hashed, persisted, or sent as an artifact uses ordered collections
  (`BTreeMap`, sorted `Vec`) and canonical serialization. Hash-map iteration
  order must never reach an artifact or hash.
- Plans are immutable once persisted. Never "refresh" a plan or a commit
  requirement in place.

### Async and I/O

- Tokio is the runtime. Never block an async thread; CPU-heavy or blocking work
  runs in the task subprocess or `spawn_blocking`, and code must not assume a
  blocking task can be cancelled.
- Never hold a database transaction across network I/O, gRPC waits, or
  object-store calls.
- Every network call has a timeout. Retries are explicit and follow the
  failure-disposition rules; no hidden retry middleware on commit requests.

### Dependencies

- Declare every dependency once in `[workspace.dependencies]` and inherit it
  with `workspace = true`. Pin exact minimum versions.
- Git dependencies are pinned by `rev`, never by branch or tag. The whole
  workspace uses one iceberg-rust revision.
- No path dependencies outside this repository. Never modify sibling checkouts
  such as `../rw-iceberg-rust` or the reference repository.
- Adding a dependency needs a reason in the pull request. Prefer the standard
  library and crates already in the tree. New Git sources must be allowed in
  `deny.toml`.

### Web UI

- `web/` follows the same rules where they apply: no secrets, no logging of
  tokens, typed errors, and tests for every behavior change. TypeScript runs
  in strict mode; lint warnings fail the gate.
- Never edit `web/src/api/schema.d.ts` by hand. After changing
  `crates/coordinator/src/api/openapi.json`, run
  `npm --prefix web run generate` in the same change.
- Declare npm dependencies with exact versions and commit
  `web/package-lock.json`. Adding one needs a reason in the pull request,
  like a Rust crate.
- Values that can exceed 2^53, such as snapshot IDs, cross the API as decimal
  strings; browsers parse JSON numbers as doubles.
- Test pages through the fake coordinator in `web/src/test/fakes.ts`, with
  fixtures typed by the generated schema, so a contract change breaks the
  tests at compile time.
- Follow ADR 0046: reuse `web/src/ui/` primitives, shared page components, and
  the theme tokens in `web/src/styles.css`. Keep components focused; extract
  reusable behavior instead of copying forms, transport, or permission logic.
  Render one responsive layout at a time from a media query rather than hiding
  a duplicate copy.
- Use the typed routes in `web/src/routes.tsx`. Validate resource identifiers
  and supported search parameters before querying; preserve namespace levels
  and other structured identifiers rather than splitting their display names.
  Keep tab and filter state that should survive a reload in validated search
  parameters.
- Use the generated client and shared transport in `web/src/api/client.ts`.
  Protect reads with `forSession`, pass the query's abort signal, and use
  `useSessionMutation` for writes so a late response cannot update another
  session. Invalidate affected queries after a successful write.
- Echo CSRF through the shared helper. Send the observed revision for edits;
  do not silently rebase a conflicted form. Never retry mutations automatically.
  Use `useKeyedMutation` when the API supports idempotency; otherwise reconcile
  an uncertain outcome before offering another potentially duplicate write.
- Match controls to the session's grant scope, while keeping the server as the
  authorization authority. No credential value belongs in a form, query
  parameter, browser storage, error, or diagnostic artifact; use references.
- Handle loading, empty, error, pending, and success states. Follow API cursors
  for lists and make partial results clear. Preserve edits on a rejected save,
  explain conflicts, and confirm destructive actions with their actual scope.
- Use semantic HTML, associated labels, keyboard-operable controls, and visible
  focus. Accessible names must include visible control text. Communicate status
  with text, never color alone, and honor reduced motion. Check light and dark
  themes, hover/focus states, narrow layouts, and long names or identifiers.
- Keep runtime requests on the deployment's origin: no CDN assets, external
  fonts, or analytics. Follow the coordinator's CSP; avoid inline styles and
  HTML injection. Keep the JavaScript and stylesheet builds within their
  budgets.
- Render server data as text. Never pass it to `innerHTML`, `eval`, or an
  `href`; build links only from typed routes. A new popup primitive or
  animation library must pass the browser gate's policy check before a page
  uses it.
- Scope aggregates by grant inside the database query: counts, facets,
  search suggestions, and event streams must not reveal resources outside the
  caller's grant. Test them with two principals on disjoint grants.
- Cap free-text query parameters and escape `LIKE` wildcards; search display
  names only, never table rows.
- Pass the query's abort signal so superseded requests cancel, keep the
  previous page visible while a filter changes, and virtualize long lists.
- Use the color, type, and motion tokens of ADR 0067: color only for status,
  two font weights, and motion only on `opacity` and `transform`.
- A shortcut (command palette, row action, peek panel) that writes opens the
  same confirmation dialog as the page.

## 6. Testing

- Unit tests live beside the code in `#[cfg(test)] mod tests`. Integration tests
  live in each crate's `tests/` directory.
- Test names describe the behavior: `cancellation_is_rejected_after_send_authorization`.
- State machines and classifiers get exhaustive tests (every state pair, every
  status branch).
- Native maintenance tests run against real Iceberg metadata and Parquet files.
  JSON models and mock transports are supporting tests only.
- Tests that need PostgreSQL, Lakekeeper, or object storage use the shared
  harness (added in BS-003 and BS-005) and must be runnable with one command.
- Tests are deterministic: no sleeps for synchronization, no reliance on wall
  clock order, no shared mutable global state.
- Coverage: the workspace stays at or above 80% line coverage. State
  transitions, planning, commit classification, and delete application aim for
  full branch coverage.
- CI runs the full Rust suite for Rust changes and the full web gate for web
  changes; `main` runs everything. Changed-area selection narrows the PR
  matrix, never the suite within an area. No nextest profile used in CI may
  set `default-filter`, because that silently skips tests.
- A test may include another crate's test support by `#[path]` only when its
  crate already depends on that crate, so change selection by dependency still
  re-runs it.

### Browser testing and Playwright MCP

- Verify frontend work in a real browser with Playwright; jsdom tests alone do
  not prove the page. Build the UI (`npm --prefix web run build`, which the gate
  serves) and drive the changed page at 320, 390, and desktop widths.
  Playwright is the preferred instrument for a frontend change: it shows
  layout, focus order, the console, network, and CSP behavior that unit tests
  cannot, so use it to inspect the actual surface as you work rather than only
  at the end.
- Add or extend browser journeys in `web/e2e/` for changed user workflows.
  Reuse `web/e2e/coordinator.ts` and contract-typed fixtures. The acceptance gate
  runs the production build in the repository-pinned Chromium, under the
  coordinator's CSP, with axe checks in both themes. It must keep detecting
  console errors, unexpected external requests, and policy violations.
- Use role and label locators and Playwright's assertions for synchronization.
  Wait for the expected UI state, not an arbitrary timeout. When checking
  contrast after opening a dialog, wait for its finite opening animations to
  finish so axe measures the visible controls rather than transparent text.
- Visual baselines live under `web/e2e/**-snapshots` and compare only under
  `make web-e2e-image`, where the pinned Linux browser renders them. Other
  screenshots are review artifacts, not source files.
- Playwright MCP is optional developer tooling for exploratory interaction,
  screenshots, console/network inspection, and responsive checks. Use it when
  available, with a dedicated test browser and fake or development data. MCP
  inspection supplements the committed acceptance tests; it does not replace
  `make web-e2e-image`, and its Chrome version need not match the pinned test
  browser.
- Do not require a machine-specific MCP configuration or extension in order to
  develop or test the repository. Keep local MCP setup outside tracked project
  configuration. `/.playwright-mcp/` is ignored; logs, screenshots, traces, and
  authenticated browser profiles are local artifacts, never source files.
- Report which checks actually ran and any remaining gates. A screenshot or
  successful Vite session alone does not establish accessibility, CSP
  compliance, generated-client freshness, or browser acceptance.

## 7. Architecture invariants

Breaking one of these is a correctness bug, whatever the tests say.

1. Workers never access PostgreSQL, never hold catalog credentials, never
   publish metadata, and never delete source files.
2. The coordinator never reads table rows.
3. Only the coordinator publishes, through the standard REST commit with the
   original planning-baseline requirements. Never run iceberg-rust's
   `Transaction::commit` against a real catalog; it reloads the table and
   re-applies actions. Commits are built with the capture catalog in
   `crates/catalog` (ADR 0015).
4. No commit is sent without the send-authorization transaction, and no
   network call happens inside that transaction.
5. An ambiguous commit is never resolved as failed or cancelled without
   conclusive evidence of non-publication.
6. Terminal jobs never transition. Retries create a new job with a fresh plan.
7. Accepted results are never reused across jobs or publications.
8. Task grants are persisted before they are returned.
9. Attempt outputs go only under
   `<data-location>/bergsmith/<job_id>/<attempt_id>/`.
10. Storage paths are built from typed `Location` values, never by string
    concatenation.

## 8. Git conventions

### Branches

- `main` is always releasable and protected. All changes reach it through pull
  requests that pass CI.
- Branch from the latest `main` as `<type>/bs-NNN-<short-slug>`, for example
  `feat/bs-003-job-transitions`. Use `chore/<slug>` or `docs/<slug>` for work
  outside the backlog.
- One task (`BS-NNN.Tn`) per branch and pull request. Keep pull requests small
  enough to review in one sitting. When the plan delivers a work package as
  a single branch (BS-033), each task is one or more commits on it, every
  commit names its task in `Refs:`, and each commit still builds and passes
  its checks.

### Commits

- Conventional Commits: `type(scope): subject`.
  - Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`, `ci`,
    `chore`.
  - Scope: the crate without the `bergsmith-` prefix (`common`, `catalog`,
    `worker`) or an area (`plan`, `adr`, `deps`, `ci`).
  - Subject: imperative mood, lowercase start, no trailing period, at most 72
    characters. Example: `feat(common): validate job state transitions`.
- Body, wrapped at 72 characters, answers What changed, Why, and How.
- Footer: `Refs: BS-NNN.Tn`. Breaking changes use `!` after the scope and a
  `BREAKING CHANGE:` footer.
- AI-assisted commits add a `Co-Authored-By:` trailer for the assistant.
- One logical change per commit. Each commit builds and passes tests on its own.
- Never commit secrets, `.env` files, credentials, build output, or coverage
  artifacts. `Cargo.lock` is always committed.

### History

- Keep history linear: rebase on `main` instead of merging `main` into a branch.
  Pull requests are squash- or rebase-merged.
- Never force-push `main` or any shared branch. `git push --force-with-lease`
  is allowed only on your own unmerged branch.
- Never bypass hooks or signing with `--no-verify` or `--no-gpg-sign`.
- Never rewrite published history, delete remote branches you don't own, or run
  destructive commands (`reset --hard`, `clean -fdx`, `checkout -- .`,
  `branch -D`) on work that isn't yours without explicit approval.
- Releases are annotated tags `vMAJOR.MINOR.PATCH` on `main`, following
  Semantic Versioning.

### Rules for coding agents

- Commit, push, open pull requests, or tag only when asked.
- Before any destructive or history-changing git command, show what it will
  affect and get confirmation.
- Never stage files you didn't change. Review `git status` and `git diff
  --staged` before committing.
- After finishing a task in a git worktree, clean it up: remove the worktree
  (`git worktree remove <path>`, or `worktree_remove` from the agent harness)
  and delete its branch once merged. Worktrees are task-local scratch
  checkouts, not permanent ones; do not leave stale ones behind. A worktree
  may only be removed after confirming its changes are committed or merged,
  and removing one with uncommitted changes needs the same confirmation as
  any destructive command.

## 9. AI agent review workflow

Local review runs through Herdr when the process is inside a managed pane.
Verify `HERDR_ENV=1` before any Herdr command; if it is unset, say that the
review cannot run through Herdr and stop rather than controlling the
maintainer's session from outside it. The reviewer is a separate agent session
in its own pane; it reads the change, runs non-destructive checks, and never
edits files. The loop continues until a round reports no actionable findings.

1. Split a pane, preserving the checkout and the caller's focus:
   `herdr pane split --current --direction right --cwd "$PWD" --no-focus`.
   Take the pane ID from the result. Start the reviewer with the agent kind the
   user provides (`herdr agent start <name> --kind <kind> --pane <pane_id>`).
   If the user did not name a kind, ask before starting; do not assume one.
   Agent names and pane IDs are local; never commit them.
2. Write a production-readiness brief: inspect the code and its context before
   concluding; prioritize defects introduced by the current diff (bugs,
   security, data integrity, concurrency, reliability, performance, API
   compatibility, missing regression tests); forbid speculative findings,
   style preferences, and edits. Require, per finding, severity P0–P3, exact
   file and line, the defect and why it occurs, a realistic failure scenario,
   and a concrete fix with a regression test, sorted by severity, plus a merge
   recommendation and the checks the reviewer actually ran.
3. Deliver the brief with `herdr agent prompt <name> "<text>" --wait --timeout
   <ms>`. A review is long, so a timeout does not prove the prompt was not
   delivered: inspect with `herdr agent get` and `herdr agent read <name>
   --source recent-unwrapped` before resubmitting, and never submit the same
   brief twice.
4. Fix or explicitly rebut every P0–P2 finding and every P3 that is a real
   defect. Commit and push before the next round so it reviews the current
   tree, and rerun the focused checks for the change.
5. Re-prompt with the previous round's findings: verify each fix, look for
   regressions those fixes introduced, then ask for anything new. Repeat until
   a round reports no actionable findings; stop there rather than extending a
   stale approval.
6. The review supplements the gates, never replaces them: run the focused
   local checks and let Actions own the selected CI gates.

## 10. Documentation

- Update the plan when a contract, scope, or default changes.
- Record each new architecture decision as an ADR (`docs/adr/NNNN-title.md`,
  from `docs/adr/template.md`). Supersede an ADR with a new one; never rewrite
  an accepted decision.
- Keep `docs/support-matrix.md` truthful: a capability is "Certified" only
  after its acceptance tests pass.
