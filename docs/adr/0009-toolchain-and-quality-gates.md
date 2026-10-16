# 0009. Stable toolchain and enforced quality gates

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4 Engineering standards and quality gates

## Context

Correctness depends on discipline across many crates and contributors, including coding agents.

## Decision

- Rust 2024 edition on stable 1.94.1, pinned in `rust-toolchain.toml`; `rust-version = "1.94"` matches iceberg-rust.
- Workspace lints: `unsafe_code` forbidden; clippy `all` and `pedantic`; `unwrap`, `expect`, `panic`, `todo`, and print macros denied outside tests.
- Gates on every pull request and in `make ci`: rustfmt, check, clippy with warnings as errors, nextest, docs with warnings as errors, at least 80% line coverage, cargo-deny, cargo-machete.
- Conventions for code, tests, and git live in `AGENTS.md`.

## Consequences

- Toolchain upgrades are explicit changes.
- Dependency direction (workers never depend on persistence or catalog crates) is enforced by `deny.toml`.
