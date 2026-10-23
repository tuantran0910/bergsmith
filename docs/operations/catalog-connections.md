# Connecting a catalog

Bergsmith maintains tables in an Iceberg REST catalog you already run. The
catalog keeps metadata authority. Bergsmith commits through the standard REST
commit with conditional requirements, so it never needs to stop your other
writers. It does need the catalog, the object store, and your other
maintenance tools to keep what an unfinished job depends on.

Read this page before you enable writes on a connection. Each item below is
an operating requirement, not a tuning hint.

## What the catalog must provide

| Requirement | Why |
|---|---|
| `GET /v1/config`, list tables, load table, and `POST` commit table | Connecting fails if the catalog does not advertise them |
| Vended, table-scoped storage credentials (`X-Iceberg-Access-Delegation: vended-credentials`) | Workers get storage access only through the grant; they never hold catalog credentials |
| `GET .../credentials` (optional) | Refreshes credentials without reloading the table |
| `idempotency-key-lifetime` (optional) | Lets Bergsmith replay an identical commit with the same key after a lost response, inside that window |

Lakekeeper v0.14.0 provides all of these. It advertises a 30-minute
idempotency window and honors replays: the same request with the same key
returns success without publishing twice.

## Checking a connection

`POST /api/v1/catalogs/{id}/check` asks the catalog what it advertises and
records the answer with the time of the check. It reads `GET /v1/config` and
nothing else, so it is safe to run against a catalog that is not ready yet.

The answer reports each requirement above as `met`, `missing`, or `unknown`:

| Requirement | Answer |
|---|---|
| List, load, and commit tables | `met` when the catalog advertises those endpoints. A catalog that advertises no endpoints serves the specification's default set, so all three are `met`. |
| Vended, table-scoped storage credentials | `unknown`. The catalog configuration cannot show this; loading a table proves it. |
| `GET .../credentials` (optional) | `met` when the catalog advertises it, `missing` otherwise. Credentials are refreshed by reloading the table when it is missing. |
| `idempotency-key-lifetime` (optional) | The advertised lifetime in seconds, as a decimal string, or `null`. |

The answer belongs to the settings it describes. Change the connection's
endpoint, warehouse, or credential reference and the recorded answer no
longer applies: it is hidden until the connection is checked again, and a
check that was running while the connection changed is refused rather than
recorded.

A check never certifies write support, credential vending, or the retention
bounds below, and it never resolves a publication. It also never changes the
connection: the check is diagnostic, so a catalog that cannot be reached
leaves the previous answer and the configuration untouched.

## What Bergsmith protects against, and what it does not

Every commit asserts the planning baseline's table UUID, `main` snapshot,
current schema ID, last assigned field ID, default partition spec ID, last
assigned partition field ID, and default sort order ID. If another writer
appends, deletes, or changes the schema, partition spec, or sort order after
planning, the catalog rejects the commit. The job ends as `ReplanRequired`,
and nothing it wrote is published.

Immediately before publishing, Bergsmith also reloads the table and treats
any observed change as `ReplanRequired`. That includes a new metadata file,
a format version change, and changes to `write.data.path`,
`write.format.default`, `write.object-storage.*`, `write.metadata.path`, or
`encryption.key-id`.

Standard REST cannot assert the metadata reference, the format version, or
arbitrary properties. So a change made between that reload and the commit
can coexist with a valid Bergsmith commit. **Do not run these concurrently
with Bergsmith jobs on the same table:**

- format version upgrades;
- encryption changes;
- table location or `write.*.path` changes;
- property changes that alter how files must be written.

Scheduling a Bergsmith job does not pause your writers. On tables written
continuously, a job succeeds only if planning, execution, and publication fit
between two writer commits.

## Where Bergsmith writes

Attempt outputs go to `<data-location>/bergsmith/<job_id>/<attempt_id>/`.
`<data-location>` is the table's `write.data.path`, or
`<table-location>/data` when that is unset. A `write.data.path` outside the
table location is refused, because table-scoped credentials cannot reach it.
Lakekeeper refuses to set `write.data.path`, so on Lakekeeper outputs are
always under `<table-location>/data/bergsmith/`.

Building a commit also writes manifests and a manifest list under the
table's metadata location. If the commit is never published, these are
orphans until cleanup reclaims them.

## Retention you must configure

A job's commit outcome can be unknown, for example after a timeout or a lost
response. Bergsmith then recovers it read-only, by finding its proposed
snapshot in retained table history. Absence of the snapshot never proves the
commit failed. So recovery depends on what you retain.

Define these bounds for each connection:

- **Maximum execution duration (E):** the job duration budget, about 3 minutes
  for a 5-minute writer cadence.
- **Maximum unresolved-publication age (U):** how long a job may stay
  `Committing` or `CommitOutcomeUnknown` before an operator investigates.

Then configure:

| Control | Requirement |
|---|---|
| Snapshot expiration (`expire_snapshots`, `history.expire.max-snapshot-age-ms`) | Retain snapshots for longer than E + U. A job's planning snapshot, and any snapshot it may have published, must not expire while the job is unresolved |
| `history.expire.min-snapshots-to-keep` | High enough that a busy table keeps a snapshot published during an unresolved job |
| Source file retention | Data files, delete files, and manifests of the planning snapshot must stay readable for at least E. Losing them fails the job; it never publishes from incomplete input |
| Orphan file cleanup (`remove_orphan_files` and similar) | Either set `older_than` above E + U, or exclude `<data-location>/bergsmith/` and the metadata location's unpublished manifests while jobs are unresolved |
| Incomplete multipart uploads | Add a bucket lifecycle rule `AbortIncompleteMultipartUpload` (ADR 0014). Bergsmith cannot list leftover uploads itself |
| Catalog idempotency window | If replays are used, the catalog must remember keys longer than the replay deadline Bergsmith records at first send |

Bergsmith's ledger stops its own cleanup from reclaiming protected objects.
It cannot make an external cleaner respect those reservations, and
neither credentials nor an attempt-unique path protect an object from a
cleaner. When you register a connection, record:

- which cleaners run on its tables;
- their retention windows;
- how the `bergsmith/` prefix is excluded, or how cleanup is suspended while
  jobs are unresolved.

## When history is gone

If the catalog is unavailable or has lost the relevant snapshot history, an
unresolved job stays unresolved and its objects are retained for an operator.
Bergsmith does not guess. Restoring the Bergsmith database from a backup
requires reconciling intents from retained table metadata before writes
resume.
