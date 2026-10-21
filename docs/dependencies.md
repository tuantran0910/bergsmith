# Dependency patches

Workspace dependency declarations retain the iceberg-rust integration baseline
`risingwavelabs/iceberg-rust@f501ff0c245b4b625fe52feaf6125c69f7132557`
for the core, REST catalog, and OpenDAL storage crates (ADR 0003). All three
resolve through a single pinned patch source, currently
`thealtoclef/rw-iceberg-rust@28b0f37c`, as recorded below (ADR 0021).

## BS-011: preserve added-file partition specs

Installed patch: [risingwavelabs/iceberg-rust#262](https://github.com/risingwavelabs/iceberg-rust/pull/262),
reapplied at [thealtoclef/rw-iceberg-rust@e7ed9e8](https://github.com/thealtoclef/rw-iceberg-rust/commit/e7ed9e8db3e09dbf39503296c3dd753b28b8a04e).

The baseline's snapshot producer rejects added files whose spec differs from
the table default, then uses the default spec for every added manifest. Native
rewrites preserve original partitions, so publishing an old-spec replacement
fails even though its Parquet rows and partition descriptor are correct.

The patch validates against each file's declared spec and groups added
manifests by spec ID in deterministic order. It preserves explicit data sequence
numbers and inherited file sequence numbers. Its upstream regression rewrites
an old-spec file while adding replacements in both old and current specs.

The workspace `[patch]` redirects all three packages to the full immutable
revision `28b0f37c446e154d95860d3851eb455d0099cac9`, which contains this fix
and the Arrow/Parquet 59 upgrade. The core, REST catalog, and storage adapter
therefore share one source revision; no local override or branch tracking is
required. The source is explicitly allowed by `deny.toml`.

The installed upstream regression additionally rejects unknown spec IDs and
incompatible partition tuples. Bergsmith's `native_publication` suite checks
actual replacement files and manifests, including old specs, explicit data/file
sequence numbers, shared and obsolete delete retention, later equality deletes,
and zero-output source removal. Its oracle uses a separate Parquet reader.

Run the complete Bergsmith gates with:

```sh
make ci
```

Validation on 2026-10-17 at the original `eb162a1` revision: all gates pass, with
453 tests passed, none skipped, and 97.10% workspace line coverage.

The published manifest implementation includes its own regression test.
Temporary local prototypes are not needed to build or validate Bergsmith.

## Equality-delete correctness and performance update

The current pin also includes two changes from the maintainer's
`dev_rebase_main_20260807` branch:

- [3a5ee4e](https://github.com/thealtoclef/rw-iceberg-rust/commit/3a5ee4e48de06b32ef07dcc2ed1b20c02f0e1d49):
  resolve struct-nested equality keys recursively by field ID when evolving
  delete batches, including renamed keys and parent null propagation.
- [8c3f269](https://github.com/thealtoclef/rw-iceberg-rust/commit/8c3f269b4abac29aad3c0101271481e40ff65a55):
  encode equality keys into hash sets and probe them in the Parquet row filter,
  replacing the predicate tree that performed work for each row/key pair;
  preserve parent nulls when materializing absent nested keys with defaults,
  including parents whose only physically stored children have been dropped
  from the current table schema.

The branch reapplies the per-spec manifest fix as `e7ed9e8` and then upgrades
the core crates to Arrow/Parquet 59.3.1. The current pin is `28b0f37c`; it is
not a linear descendant of the previous pin. All three packages move together
to that full immutable SHA. No branch tracking or local checkout override is
used.

The existing native rewrite and publication suites remain the correctness
checks, including nullable single/composite keys, sequence-number boundaries,
schema evolution, partition scope, old-spec publication, and zero-output groups.
Keep the 50,000-key oracle regression at its full size; measure its runtime
without adding a machine-dependent timing assertion. The original release-mode
BS-010 measurements remain historical evidence, not measurements of this pin.

Measured on 2026-10-18 on the same local machine with Rust 1.94.1:

```sh
cargo test --locked -p bergsmith-maintenance --test rewrite \
  fifty_thousand_equality_keys_match_the_oracle -- --exact --nocapture
```

The unchanged test passed in 69.27 seconds at `eb162a1` and 2.29 seconds at
`e7ed9e8`, about 30 times faster. Both runs used the debug test profile;
reported test times exclude compilation and include fixture/oracle work.
This single-sample comparison is not a production throughput benchmark.

All four independent real-Parquet review cases pass at `e7ed9e8`: renamed nested
keys, additional non-key siblings, defaulted keys under nullable parents, and
parents whose only stored child was dropped. Parent presence is now resolved by
field ID in the physical file schema, retaining a physical descendant to decode
the parent's validity. The upstream commits also include regression tests.

Validation on 2026-10-18 at `e7ed9e8`: `make ci` passes all 535 tests, none
skipped, with 95.84% workspace line coverage. All other required gates pass.

## CVE-2026-43868 and Arrow/Parquet 59.3.1

Dependabot alert GHSA-2f9f-gq7v-9h6m / CVE-2026-43868 flags `thrift < 0.23.0`
(memory allocation with excessive size, moderate). Parquet 58.4.0 requires
`thrift ^0.17`; Bergsmith inherited it through the pinned iceberg-rust core
crates.

The fork's `dev_rebase_main_20260807` branch now pins Arrow and Parquet 59.3.1,
which removes Parquet's Thrift dependency, and updates the DataFusion
integration to 55 (and pyo3 to 0.29). All three iceberg-rust packages stay on
one revision. `cargo tree -i thrift` in Bergsmith reports no such package.

Validation on 2026-10-21 at `28b0f37c`: `make ci` passes all 760 tests, none
skipped, with 90.59% workspace line coverage. The native rewrite, delete,
publication, and distributed acceptance suites pass, as do the web build and
audit. Fork-side `check-fmt`, `check-toml`, `cargo machete`, `check-clippy`,
unit/doc tests, and `check-public-api` pass. The fork's Docker-backed
integration suite was not completed locally after a `ghcr.io` image pull
failure; Bergsmith's gates do not depend on that suite.

Removal condition: when the integration branch contains all installed fixes,
advance all three workspace dependencies to that reviewed revision and remove
the redirect. Retain Bergsmith's regression tests. Changes to the fork or PR #262
after the pinned commit require a deliberate dependency update.
