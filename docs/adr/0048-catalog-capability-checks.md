# 0048. Capability checks for catalog connections

- Status: Proposed
- Date: 2026-10-23
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-022.T2

## Context

`docs/operations/catalog-connections.md` lists what a catalog must provide
before Bergsmith can maintain its tables. Until now an operator could only
learn that a catalog was unsuitable when a job failed, and a connection
created through the browser API was never checked at all.

The REST catalog answers one question that covers most of those
requirements: `GET /v1/config` advertises the endpoints it serves and the
idempotency-key lifetime it keeps. Two requirements cannot be answered that
way: vended storage credentials appear in a table load, not in the
configuration, and the credentials endpoint is optional.

## Decision

`POST /api/v1/catalogs/{id}/check` reads `GET /v1/config` on the connection's
catalog, records the answer with the time of the check, and answers with it.
The check is diagnostic: it never changes the connection's configuration, never
moves its revision, and it deletes nothing.

The check reports what the catalog lacks instead of refusing it. A catalog
that is missing a required endpoint is recorded with that requirement
`missing`, which is what an operator needs; the catalog client used for work
still refuses such a catalog.

Each requirement is `met`, `missing`, or `unknown`. Vended credentials stay
`unknown`: the catalog configuration cannot show whether a catalog vends
table-scoped storage credentials, and loading a table proves it. The
credentials endpoint and the idempotency-key lifetime are optional
requirements, and their absence is `missing`. The advertised lifetime crosses
the API as a decimal string, so the value is exact even when it is larger
than a browser counts.

The answer is bound to the settings it describes. The check stores a
fingerprint of the routing it asked about, and it stores nothing when the
connection's routing changed while the catalog was answering: the answer would
describe a configuration that no longer exists. Such a check returns 409
`catalog_connection_changed` and the operator runs it again. For the same
reason, a recorded answer is not shown with the connection after a routing
edit. The fingerprint tags and length-prefixes every setting, so an absent
setting and an empty one never hash alike: a connection without a warehouse
sends no warehouse parameter, while one with an empty warehouse sends an
empty value. The comparison and the write happen under a shared row lock, so
an edit cannot land between them, and the catalog call itself happens outside
every transaction.

The check requires `connections:manage` for the connection. The ledger
connection is released before the catalog is called, and no database lock
spans that call; the answer and its `catalog.checked` audit record commit
together afterwards. The audit record names the missing requirements and
nothing else: no URI, warehouse, credential reference, or catalog content.

A check is bounded four times. The catalog's answer may not exceed 64 KiB,
and the body is read in chunks so the limit holds for a chunked answer as well;
a larger answer is refused with `catalog_response_too_large` rather than
buffered. The check itself is given up after ten seconds, the connection after
five, and one catalog request after thirty. The coordinator runs at most 8
checks at once and at most 2 for one requester; a check that cannot start is
refused with 503 `capability_check_capacity_exhausted` instead of waiting, so
a faulty catalog or a burst of checks cannot exhaust the process. A requester
takes its permit while the map of requesters is locked, so one requester can
never hold two gates and exceed its own bound; a gate nobody holds a permit
on may be dropped, which keeps that map from growing without end.

A catalog that cannot be reached leaves no record and returns a transient
error. A connection that names an external credential cannot be checked until
the runtime resolves credential references (ADR 0035); the coordinator never
falls back to ambient identity, so a catalog that requires authentication
answers 401 and the check reports `catalog_access_denied`.

## Consequences

An operator can tell whether a connection can serve work before trusting it,
and the connection page (BS-022.T5) can show each requirement in
`docs/operations/catalog-connections.md` as met, missing, or unknown. A check
is a snapshot: the next check replaces it, and the audit log keeps the
history. A check never certifies write support, credential vending, or the
retention a connection needs, and it never resolves a publication. A recorded
answer is hidden once the routing it describes changes, so an operator never
reads an answer as if it described the connection in front of them.
