-- BS-003.T2: organizations, principals, catalog connections, and canonical
-- table identities.
--
-- Every row is scoped to one organization. Composite foreign keys on
-- (organization_id, id) stop a row from referencing another organization's
-- data.

CREATE TABLE organizations (
    id uuid PRIMARY KEY,
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    display_name text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A person (OIDC user) or a service account, identified by issuer and subject.
CREATE TABLE principals (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    kind text NOT NULL CHECK (kind IN ('User', 'Service')),
    issuer text NOT NULL CHECK (length(issuer) > 0),
    subject text NOT NULL CHECK (length(subject) > 0),
    display_name text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, issuer, subject),
    UNIQUE (organization_id, id)
);

-- A connection to an Iceberg REST catalog. `credential_ref` names a secret
-- held outside PostgreSQL; the secret itself is never stored here.
CREATE TABLE catalog_connections (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    name text NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    uri text NOT NULL CHECK (uri ~ '^https?://'),
    warehouse text,
    credential_ref text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, name),
    UNIQUE (organization_id, id)
);

-- A canonical table, keyed by its Iceberg `table-uuid`. All maintenance
-- coordination (jobs, publication barriers) attaches to this identity.
CREATE TABLE table_identities (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    table_uuid uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, table_uuid),
    UNIQUE (organization_id, id)
);

-- A catalog name for a table. Aliases that resolve to the same `table-uuid`
-- share one identity, so they share coordination.
CREATE TABLE table_aliases (
    catalog_connection_id uuid NOT NULL,
    namespace text[] NOT NULL CHECK (cardinality(namespace) > 0),
    table_name text NOT NULL CHECK (length(table_name) > 0),
    organization_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (catalog_connection_id, namespace, table_name),
    FOREIGN KEY (organization_id, catalog_connection_id)
        REFERENCES catalog_connections (organization_id, id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id)
);

CREATE INDEX table_aliases_by_identity ON table_aliases (table_identity_id);
