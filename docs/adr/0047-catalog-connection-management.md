# 0047. Revision-checked catalog connection management

- Status: Proposed
- Date: 2026-10-22
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-022.T1

## Context

Catalog connections previously had read-only browser resources and an operator
creation command. Editing a connection must respect scoped administration and
must preserve the catalog authority needed to resolve an ambiguous publication.

## Decision

`POST /api/v1/catalogs` requires organization-wide `connections:manage`.
`PATCH /api/v1/catalogs/{id}` requires that permission over the connection and
an `expected_revision`. Each mutation and its audit record commit together.
Existing connections remain enabled at revision zero after migration.

PATCH can change the name, URI, warehouse, external credential reference, and
enabled state. Omitted fields are preserved; null clears warehouse or the
credential reference. Validation applies to supplied fields only; legacy values
do not prevent renaming or disabling. A PATCH with no editable fields is
rejected. Supplied URIs with user information, query parameters, or fragments
are rejected. Credential references are identifiers, never credential values.
Only administrators receive connection settings from the detail resource;
inventory lists and other readers receive identity, enabled state, revision,
and timestamps. Detail responses strip URI user information, query, and fragment
from legacy operator-created rows; an unparseable legacy URI is omitted.
Audit records contain revision and enabled state only.

Disabling prevents new previews and execution admissions through that
connection. A due schedule with no enabled alias its service identity may use
records `ConnectionDisabled` and advances its cursor without creating a job.
Another enabled alias can be used only if the requester may submit through it;
runtime planning and existing execution prefer that enabled route too, falling
back to a disabled alias when no enabled route is available for existing work.
Existing execution admissions, commit preparation, and publication recovery
continue. No connection, alias, job, or publication evidence is deleted.

Admission takes shared connection row locks; configuration updates take the
exclusive row lock. Job creation also takes shared locks in connection-ID
order. URI, warehouse, and credential-reference edits return
`catalog_connection_in_use` while a nonterminal job belongs to a table aliased
on the connection. This protects recovery, which currently reloads these
settings from the connection named by the immutable intent. Name and enabled
state can still change. No database lock spans external I/O.

## Consequences

Disabling is an admission control; operators cancel existing jobs explicitly.
Connection routing changes must wait for dependent jobs to resolve, including
unknown publication outcomes. A bad endpoint cannot be changed through this
API while such jobs remain; repair the service or DNS at its existing address.
Credential-reference resolution remains a separate runtime limitation; saving
a reference does not certify that the deployment can resolve it. Capability
checks, per-connection retention settings, discovery, and pages remain later
BS-022 tasks.
