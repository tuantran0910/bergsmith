# Dependency patches

Workspace dependency declarations retain the iceberg-rust integration baseline
`risingwavelabs/iceberg-rust@f501ff0c245b4b625fe52feaf6125c69f7132557`
for the core, REST catalog, and OpenDAL storage crates (ADR 0003). All three
resolve through a single pinned patch source, as recorded below (ADR 0021).

## BS-011: preserve added-file partition specs

Installed patch: [risingwavelabs/iceberg-rust#262](https://github.com/risingwavelabs/iceberg-rust/pull/262),
from [thealtoclef/rw-iceberg-rust@eb162a1](https://github.com/thealtoclef/rw-iceberg-rust/commit/eb162a1e109dbd2961d1fcd3e93cc8c770120bb9).

The baseline's snapshot producer rejects added files whose spec differs from
the table default, then uses the default spec for every added manifest. Native
rewrites preserve original partitions, so publishing an old-spec replacement
fails even though its Parquet rows and partition descriptor are correct.

The patch validates against each file's declared spec and groups added
manifests by spec ID in deterministic order. It preserves explicit data sequence
numbers and inherited file sequence numbers. Its upstream regression rewrites
an old-spec file while adding replacements in both old and current specs.

The workspace `[patch]` redirects all three packages to the full immutable
revision `eb162a1e109dbd2961d1fcd3e93cc8c770120bb9`. The core, REST catalog,
and storage adapter therefore share one source revision; no local override or
branch tracking is required. The source is explicitly allowed by `deny.toml`.

The installed upstream regression additionally rejects unknown spec IDs and
incompatible partition tuples. Bergsmith's `native_publication` suite checks
actual replacement files and manifests, including old specs, explicit data/file
sequence numbers, shared and obsolete delete retention, later equality deletes,
and zero-output source removal. Its oracle uses a separate Parquet reader.

Run the complete Bergsmith gates with:

```sh
make ci
```

Validation on 2026-10-17 at the pinned revision: all gates pass, with 453 tests
passed, none skipped, and 97.10% workspace line coverage.

The published PR commit is the installed implementation and includes its own
test. Temporary local prototypes are not needed to build or validate Bergsmith.

Removal condition: when the integration branch contains the fix, advance all
three workspace dependencies to that reviewed revision and remove the redirect.
Retain Bergsmith's regression tests. Changes to PR #262 after the pinned commit
require a deliberate dependency update.
