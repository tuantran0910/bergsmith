# Bergsmith

Bergsmith is a self-hosted maintenance platform for Apache Iceberg tables. It
compacts small files and applies position and equality deletes natively in
Rust, then publishes the result through standard Iceberg REST conditional
commits.

**Status:** native planning and local execution (milestone M2). End-to-end
platform certification is pending; see
[`docs/support-matrix.md`](docs/support-matrix.md).

## How it works

- A **coordinator** plans jobs, schedules tasks from an in-memory queue,
  persists every grant to PostgreSQL, and is the only component that commits to
  the catalog.
- **Workers** pull tasks over gRPC and run each one in a supervised subprocess
  that reads source files, applies deletes, and writes new Parquet files.
- **PostgreSQL** is the durable execution ledger. There is no message broker.
- The **Iceberg REST catalog** (Lakekeeper first) keeps metadata authority and
  vends the storage credentials workers use.

Decisions are recorded in [`docs/adr/`](docs/adr/). The full project plan is a
private maintainer document and is not part of this repository.

## Repository layout

```text
crates/
  common/        typed IDs, lifecycle states, failure dispositions
  protocol/      protobuf contracts
  catalog/       Iceberg REST integration and commit adapter
  storage/       backend-neutral object storage
  maintenance/   planner, executor, delete application
  persistence/   PostgreSQL repositories and migrations
  coordinator/   coordinator services
  worker/        worker supervisor and task subprocess
bin/
  coordinator/   bergsmith-coordinator binary
  worker/        bergsmith-worker binary
deploy/          Docker (images, Compose) and k8s assets
docs/            ADRs, support matrix, operator docs, and measurements
```

## Development

Prerequisites:

- [rustup](https://rustup.rs). The pinned toolchain in `rust-toolchain.toml`
  installs on first use.
- `cargo install --locked cargo-nextest cargo-llvm-cov cargo-deny cargo-machete`
  (or `cargo binstall` the same tools). Nextest must be at least the version in
  `.config/nextest.toml`; CI installs exactly that version.
- Docker, for the PostgreSQL, RustFS, and Lakekeeper services that `make test`
  starts (`make services-down` stops them).

Common commands:

```sh
make fmt        # format
make test       # run tests
make test-changed  # run tests of packages affected since origin/main
make ci         # run every quality gate: fmt, check, clippy, tests, docs,
                # coverage (>= 80% lines), cargo-deny, cargo-machete
```

The [local execution command](docs/operations/local-execution.md) runs a verified
filesystem plan and writes receipts without a coordinator or catalog publication.
The [native executor benchmark](docs/measurements/bs-010-executor.md) records
independent row checks, execution time and process RSS for all eleven spike fixtures.

Read [`AGENTS.md`](AGENTS.md) before contributing. It covers code conventions,
testing rules, architecture invariants, and git conventions.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
