# 0050. Catalog discovery and table registration

- Status: Proposed
- Date: 2026-10-24
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-022.T4

## Context

A connection (ADR 0047) names a catalog; Bergsmith only works on tables it has
registered, because a canonical table identity is what jobs, plans, grants, and
publication barriers attach to. Until now registration existed only as the
`register-table` operator command: it loads one table by name and resolves the
alias. An operator who wanted to see what a catalog holds had to read it
themselves, and a namespace with twenty tables took twenty commands.

The browser needs the other half of that workflow: see a connection's
namespaces, see a namespace's tables with the ones already tracked, register
one table or a whole namespace, and remove a name again. A catalog is an
external system, so each of those steps is an outbound call the coordinator
makes with the connection's credentials.

## Decision

Three read-only and two write operations, all under `connections:manage` for
the connection:

- `GET /api/v1/catalogs/{id}/namespaces` lists the namespaces a catalog holds
  under a parent, or at the top level.
- `GET /api/v1/catalogs/{id}/tables` lists one namespace's tables and marks
  the ones the connection already has an alias for.
- `POST /api/v1/catalogs/{id}/registrations` registers one table or every
  table of a namespace, and `DELETE` on the same path removes one alias.

A namespace is carried as repeated query parameters (`?namespace=a&namespace=b`)
and never as a dotted path: a catalog level may contain any character, and the
catalog itself joins levels with the unit separator.

A browse reads one page and answers with the catalog's own continuation token
as an opaque `next_cursor`, bound to the caller, the connection, and the
listing it was issued for. The namespace is bound by digest, so a deep
namespace never grows the cursor; a token too long to carry is reported as
`catalog_token_too_large` rather than returned as a cursor the next request
would have to refuse. The token is the catalog's, so Bergsmith never
orders a listing it does not own, and every entry stays reachable however large
a namespace is — a name-based cursor would have had to assume the catalog sorts
its pages, which the REST specification does not promise.

The answer carries only identifiers Bergsmith can address: a level is never
empty, never over 255 bytes, never carries a control character, and a namespace
has at most 16 levels. Anything else is counted in `skipped` and left out,
because the API would refuse it anyway. One page is bounded to 1000 items and
1 MiB of identifiers; a catalog that answers more is refused with
`catalog_page_too_large` rather than have the remainder silently dropped. That
bound is what keeps a hostile catalog from making one browse retain hundreds of
megabytes: only one page is held at a time, and only its addressable names are
kept.

Registration is bounded, idempotent, audited, and atomic:

- A catalog that answers a namespace browse with a table outside the requested
  namespace is refused with `catalog_namespace_mismatch`: recording that name
  would register a table the request never asked for.
- A namespace is read page by page, under a page bound, a byte bound, and the
  table bound, before anything is written: a namespace that spans pages is one
  request, and every page is checked for names Bergsmith cannot address.
- One request records at most 100 tables. A namespace with more is refused
  rather than half-registered, and so is a namespace holding a name Bergsmith
  cannot address (`registration_incomplete`), which would otherwise be left
  out without the operator noticing.
- Every named table is loaded from the catalog before anything is written, so
  a table the catalog cannot read, or one whose name now reaches a different
  table, leaves the whole request unrecorded.
- The aliases and their audit record commit in one transaction.
- The names belong to the routing they were read with: an edit that lands while
  the catalog is being read records nothing and returns
  `catalog_connection_changed`, the same rule the capability check follows.
- Registering a name that is already registered answers with the identity the
  first request recorded and `created: false`; it writes nothing and records
  no audit entry.

Browsing and loading share one process-wide bound of four concurrent
operations, and one registration loads at most eight tables at once. The
catalog's own connect and request timeouts bound each call. There is no
per-requester bound: the permission is administrator-only, and every operation
is already bounded in items, so a second administrator shares the capacity
instead of being isolated from it.

Removing an alias keeps the canonical table and the table's other names. The
identity is never deleted: jobs and publication evidence reference it, and
publication recovery resolves a table's name through the connection its
immutable intent names. For that reason every alias of a table with
nonterminal jobs is kept, and a removal is refused with `table_in_use` until
the work ends; the removal holds the connection the way job creation does, so
no job can be admitted between that check and the deletion.

A name is not a stable identifier: it is removed and registered again when a
table is dropped and recreated, or when a different table takes the name.
Every alias row therefore carries a revision from a sequence, and a removal
must name the revision it observed. The revision comes from a browse, and the
canonical table resource carries it too, so a name the catalog no longer lists
— the table was dropped — stays removable from the ledger rather than becoming
stuck because no browse can report it. The comparison happens under the row lock,
so a delayed removal of a row that is gone removes nothing and answers
`revision_conflict`; the replacement stays.

A disabled connection can still be browsed and registered: disabling stops new
work (ADR 0047), and registration starts none. The catalog call itself fails if
the connection's credentials are unusable, which is reported as it is.

## Consequences

The operator workflow that started with `register-table` now exists in the
browser, and a table an operator registers is the same canonical identity the
scheduler would have resolved. The `register-table` command keeps working: it
is the same `resolve_table` path without the browser bounds.

Browsing continues through the catalog's own tokens, so a namespace of any
size is reachable. A cursor is as long-lived as the catalog's token: once the
catalog expires it, the browse restarts from the first page, which the client
shows as a refused cursor rather than a wrong answer. Searching within a
namespace is left to a later change if operators ask for it.

Registration of a namespace is synchronous: the request holds a catalog client
per table, bounded as above, and the browser waits. A catalog that answers
slowly makes the request slow rather than turning it into background work,
which keeps the operator's view of what is registered accurate.

The removal guard is conservative: any nonterminal job on the table keeps all
of the table's names, not only the name on the connection the job was planned
through. Most tables have one name, so the difference is invisible; a table
with several names waits until its work finishes.

A caller that does not read the revision first cannot remove a name: the
revision is required, so an older client that removes by name alone is refused
instead of racing a replacement.

Follow-ups: expose the registered tables on the connection page (BS-022.T5),
and let a namespace browse filter by a name prefix if operators need it.
