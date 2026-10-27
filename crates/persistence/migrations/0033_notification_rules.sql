-- BS-029.T2: notification rules (ADR 0060).
--
-- A rule narrows one event kind to one destination and, optionally, to one
-- connection, worker pool, or table. The destination and the table or
-- connection filters are organization-scoped foreign keys, so a rule can
-- never point outside its organization. A worker pool is a name (worker
-- sessions carry it), not a registry row. At most one filter may be set.
CREATE TABLE notification_rules (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    name text NOT NULL CHECK (name ~ '^[A-Za-z0-9][A-Za-z0-9 ._-]{0,199}$'),
    destination_id uuid NOT NULL,
    event_kind text NOT NULL CHECK (event_kind IN (
        'job.failed',
        'job.replan_required',
        'publication.commit_outcome_unknown',
        'schedule.occurrence_missed',
        'table.health_critical',
        'workers.pool_without_workers')),
    catalog_connection_id uuid,
    worker_pool text
        CHECK (worker_pool IS NULL OR worker_pool ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    table_identity_id uuid,
    -- A repeat threshold applies only to conditions that recur; the
    -- coordinator checks the event kind against this bound before insert.
    repeat_threshold integer CHECK (repeat_threshold IS NULL OR repeat_threshold >= 1),
    enabled boolean NOT NULL DEFAULT true,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_by uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, name),
    FOREIGN KEY (organization_id, destination_id)
        REFERENCES notification_destinations (organization_id, id),
    FOREIGN KEY (organization_id, catalog_connection_id)
        REFERENCES catalog_connections (organization_id, id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id),
    FOREIGN KEY (organization_id, created_by)
        REFERENCES principals (organization_id, id),
    CONSTRAINT notification_rules_single_filter
        CHECK (num_nonnulls(catalog_connection_id, worker_pool, table_identity_id) <= 1)
);

-- Listing reads `organization_id = $1 AND id > $2 ORDER BY id`; the
-- `UNIQUE (organization_id, id)` constraint already provides that index.
