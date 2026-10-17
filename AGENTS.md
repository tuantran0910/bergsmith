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
| `crates/persistence` | SQLx repositories and migrations |
| `crates/coordinator` | Coordinator services |
| `crates/worker` | Worker supervisor and task subprocess |
| `bin/coordinator`, `bin/worker` | Thin binaries: startup wiring only |
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
| `make doc` | Build docs with warnings as errors |
| `make coverage` | Enforce at least 80% line coverage (binaries excluded) |
| `make coverage-html` | HTML report in `target/llvm-cov/html` |
| `make deny` | Advisories, licenses, bans, sources |
| `make machete` | Unused dependencies |
| `make ci` | Every gate above, in CI order (coverage runs the tests, and clippy type-checks every target) |
| `make services-up` / `make services-down` | Start or discard PostgreSQL, RustFS, and Lakekeeper (Docker Compose) |
| `make proto-lint` / `make proto-breaking` | buf lint and breaking-change checks (Docker) |

Required tools: rustup (the toolchain comes from `rust-toolchain.toml`),
`cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`, `cargo-machete`, and Docker.

Integration tests use the Compose services in `deploy/docker/`, which `make
test` and `make coverage` start first. They read `DATABASE_URL`,
`BERGSMITH_TEST_S3_ENDPOINT`, and `BERGSMITH_TEST_CATALOG_URL`; override them
to use your own servers. The PostgreSQL role must be allowed to create
databases, because `#[sqlx::test]` gives each test a fresh one.

## 4. Definition of done

A change is done only when all of these hold:

1. `make ci` passes locally. CI runs the same gates on every pull request.
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
  enough to review in one sitting.

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

## 9. Documentation

- Update the plan when a contract, scope, or default changes.
- Record each new architecture decision as an ADR (`docs/adr/NNNN-title.md`,
  from `docs/adr/template.md`). Supersede an ADR with a new one; never rewrite
  an accepted decision.
- Keep `docs/support-matrix.md` truthful: a capability is "Certified" only
  after its acceptance tests pass.
