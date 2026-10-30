-- Saved list views (BS-033.T6, ADR 0067).
--
-- A person names a list's filters, search, order, and hidden columns. The
-- parameters are the list's own URL search parameters as a JSON object of
-- strings; the coordinator checks the names against the list before writing,
-- and these constraints bound what any writer can store.

CREATE TABLE saved_views (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    principal_id uuid NOT NULL,
    page text NOT NULL
        CHECK (page IN ('jobs', 'tables', 'catalogs', 'audit', 'principals')),
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    params jsonb NOT NULL
        CHECK (jsonb_typeof(params) = 'object' AND octet_length(params::text) <= 8192),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (organization_id, principal_id) REFERENCES principals (organization_id, id),
    CONSTRAINT saved_views_name_unique UNIQUE (organization_id, principal_id, page, name)
);

-- A person's views of one list, in creation order; also the count behind the
-- per-person limit.
CREATE INDEX saved_views_by_owner ON saved_views (organization_id, principal_id, page, id);
