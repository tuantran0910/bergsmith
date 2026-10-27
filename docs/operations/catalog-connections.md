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
| `GET /v1/namespaces` (optional) | Browsing a connection's namespaces in the browser; without it, a table is registered by its full name |
| `idempotency-key-lifetime` (optional) | Lets Bergsmith replay an identical commit with the same key after a lost response, inside that window |

Lakekeeper v0.14.0 provides all of these. It advertises a 30-minute
idempotency window and honors replays: the same request with the same key
returns success without publishing twice.

## Checking a connection

In the browser, open **Catalogs → Add connection**, enter its name, HTTP(S)
catalog URI, and optional warehouse, then **Check capabilities**. This checks
the unsaved settings and creates no connection. Review missing requirements
before choosing **Save connection**. Changing any field requires a new check.
The diagnostic does not prove credential vending or certify write support.
The guided flow currently requires a catalog that accepts unauthenticated
requests: credential reference resolution is still deferred. A failed check
does not offer Save. If a save's outcome is uncertain, inspect the catalog list
before creating another connection.

The connection detail shows its settings, read-only effective storage profile,
unresolved publications, and registered tables. **Save settings** uses the
revision read; a conflict requires **Reload saved settings** before editing
again. **Edit routing** changes the URI, warehouse, or credential reference
without resending unchanged legacy fields. Only a connection administrator sees
these controls; adding a connection requires an organization administrator.

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

## Browsing and registering tables

On the connection detail, choose **Discover tables**, select a namespace, and
register a table or its whole namespace. Use **Load more** to follow each
listing. **Register by name** accepts separate namespace levels when browsing
is unavailable; a dot inside a level is kept literal. Registered tables come
from the ledger, so a name dropped from the catalog can still be removed with
**Remove registration**. Confirming removal sends the alias revision observed;
an active job or a replaced alias refuses the removal. No table data is deleted.

A connection only works on tables Bergsmith has registered: a registered name
is what a canonical table identity, its grants, and its publication history
attach to. The browser reads the catalog through the connection and records a
name when you choose it:

| Request | What it does |
|---|---|
| `GET /api/v1/catalogs/{id}/namespaces` | Lists the namespaces the catalog holds under a parent (`?parent=a&parent=b`), or the top level without one. |
| `GET /api/v1/catalogs/{id}/tables` | Lists one namespace's tables and marks the ones this connection already has, with the revision a removal names. |
| `POST /api/v1/catalogs/{id}/registrations` | Registers one table (`{"namespace": ["db"], "table": "events"}`) or every table of a namespace (`{"namespace": ["db"]}`). |
| `DELETE /api/v1/catalogs/{id}/registrations?namespace=db&table=events&revision=7` | Removes the name the caller observed. |

A namespace is a list of levels, sent as repeated parameters and never as a
dotted path: a level may contain any character. A browse reads one page and
carries the catalog's own `next_cursor`, so a namespace of any size stays
reachable without Bergsmith assuming an order the catalog never promised.

Registering loads each table from the catalog first and records the names in
one transaction; a namespace is read page by page, so a table on a later page
of a paged catalog is registered with the others. The names belong to the routing they were read with: if the
connection's endpoint, warehouse, or credential reference changes while the
catalog is being read, nothing is recorded and the request is refused, so it
is run again. A table the catalog cannot read, or a name that now reaches a
different table (a table was dropped and recreated under the same name),
leaves the whole request unrecorded. One request records at most 100 tables; a
namespace with more is refused, and its tables are registered individually.
Registering a name that is already registered changes nothing and answers with
the identity the first request recorded.

The lists a browse returns only contain names Bergsmith can address; anything
else is counted in `skipped`, and a whole-namespace registration that would
leave one out is refused.

Removing a name keeps the table and its other names. The removal names the
revision the browse reported — or the alias revision on the table's page, for
a name the catalog no longer lists — so a request that was delayed while the
name was removed and registered again changes nothing and is told to read the
name again. A table with unfinished work keeps all of its names until the work ends,
because publication recovery verifies a commit through the connection the job
planned against. The table itself is never deleted: its jobs and publication
evidence still reference it.

A disabled connection can still be browsed and registered; disabling stops new
work, and registration starts none.

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

Record both with the connection, so Bergsmith holds them next to the catalog
they describe:

```
PATCH /api/v1/catalogs/{id}
{"expected_revision": 3, "max_execution_seconds": 180, "max_unresolved_publication_seconds": 3600}
```

Bergsmith keeps its own work inside those bounds. A job never runs longer than
E, so the inputs you keep readable for E are enough; a job that outruns the
budget is given up. The budget covers the whole job: every attempt, every
retry, and every lease renewal must fit before it, and it is counted from the
moment the plan is recorded, which is right after the planning snapshot was
captured. A publication that was already authorized is never stopped by the
budget. The connection page shows how many publications of that connection are
unresolved, how old the oldest is, and whether it is past U.

Then configure:

| Control | Requirement |
|---|---|
| Snapshot expiration (`expire_snapshots`, `history.expire.max-snapshot-age-ms`) | Retain snapshots for longer than E + U. A job's planning snapshot, and any snapshot it may have published, must not expire while the job is unresolved |
| `history.expire.min-snapshots-to-keep` | High enough that a busy table keeps a snapshot published during an unresolved job |
| Source file retention | Data files, delete files, and manifests of the planning snapshot must stay readable for at least E. Losing them fails the job; it never publishes from incomplete input |
| Orphan file cleanup (`remove_orphan_files`, external cleaners) | The built-in job never lists `<data-location>/bergsmith/`, keeps every referenced object and the metadata log, and ages objects by the larger of its minimum age and E + U. An external cleaner must exclude `<data-location>/bergsmith/` and use `older_than` above E + U while jobs are unresolved |
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

## Pausing maintenance on one connection

A connection can be taken out of service entirely, or only have its maintenance
paused:

```
PATCH /api/v1/catalogs/{id}
{"expected_revision": 4, "maintenance_enabled": false}
```

A paused connection stays in service. Previews, reads, and the recovery of an
unresolved publication continue, and the connection page still shows its
capabilities. No new maintenance job is admitted and no scheduled occurrence
creates one; a job that was already admitted continues. Use the pause for a
window in which the table must keep serving reads but must not be rewritten.

Disabling the connection is the stronger lever: it stops previews as well, so
use it when the connection itself is wrong.

## Where the outputs go

The storage profile that serves a connection stays in the coordinator
configuration, so it is shown read-only on the connection page. The page names
the backends that serve the connection, and whether the connection has a
profile entry of its own or uses the default set; a connection entry that is
missing shows as the default set.

## When history is gone

If the catalog is unavailable or has lost the relevant snapshot history, an
unresolved job stays unresolved and its objects are retained for an operator.
Bergsmith does not guess. Restoring the Bergsmith database from a backup
requires reconciling intents from retained table metadata before writes
resume.
