-- BS-019.T1: maintenance policies and schedules.
--
-- A policy is the reusable "what": one operation and its bounded parameters,
-- owned by an organization and independent of any table. A schedule binds a
-- policy to one canonical table, a timing contract (five-field cron in an IANA
-- time zone), and the service principal that executes it. Windows and per-run
-- budgets arrive with BS-019.T3; occurrence rows, with BS-019.T2.
--
-- The coordinator validates cron syntax, the time-zone name, and the service
-- identity's grant before inserting. The checks here bound storage and pin the
-- names the code decodes; timestamps are advisory and have no ordering check,
-- because a wall-clock step backwards must not reject a valid update.

CREATE TABLE maintenance_policies (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    name text NOT NULL CHECK (name ~ '^[A-Za-z0-9][A-Za-z0-9 ._-]{0,199}$'),
    operation text NOT NULL CHECK (operation IN ('Compact')),
    target_file_size_bytes bigint NOT NULL
        CHECK (target_file_size_bytes BETWEEN 1048576 AND 1073741824),
    min_input_files integer NOT NULL CHECK (min_input_files BETWEEN 2 AND 1000),
    enabled boolean NOT NULL DEFAULT true,
    -- Optimistic concurrency: every update names the revision it read.
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_by uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, name),
    FOREIGN KEY (organization_id, created_by)
        REFERENCES principals (organization_id, id)
);

-- Listing reads `organization_id = $1 AND id > $2 ORDER BY id`; the
-- `UNIQUE (organization_id, id)` constraint already provides that index.

CREATE TABLE maintenance_schedules (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    policy_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    -- Canonical five-field cron text and an IANA zone name. The coordinator
    -- parses both before insert; these lengths only bound what is stored.
    cron_expression text NOT NULL CHECK (length(cron_expression) BETWEEN 5 AND 200),
    time_zone text NOT NULL CHECK (length(time_zone) BETWEEN 1 AND 64),
    service_principal_id uuid NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_by uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    -- A policy runs at most once per table. Changing the timing updates this
    -- row rather than adding a second one, so one table never gets two runs of
    -- the same policy at the same time.
    UNIQUE (organization_id, policy_id, table_identity_id),
    UNIQUE (organization_id, id),
    FOREIGN KEY (organization_id, policy_id)
        REFERENCES maintenance_policies (organization_id, id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id),
    FOREIGN KEY (organization_id, service_principal_id)
        REFERENCES principals (organization_id, id),
    FOREIGN KEY (organization_id, created_by)
        REFERENCES principals (organization_id, id)
);

-- As with policies, `UNIQUE (organization_id, id)` already indexes listing.
CREATE INDEX maintenance_schedules_by_policy
    ON maintenance_schedules (organization_id, policy_id);
CREATE INDEX maintenance_schedules_enabled
    ON maintenance_schedules (organization_id, id) WHERE enabled;
