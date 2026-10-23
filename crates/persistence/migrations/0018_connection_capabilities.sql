-- BS-022.T2: the latest capability check of a catalog connection. A check is
-- diagnostic: it records what the catalog advertised, the settings it was
-- asked about, and when, and never changes the connection's configuration or
-- revision. The fingerprint binds the answer to the routing it describes, so
-- an answer recorded under older settings is stale.
CREATE TABLE catalog_connection_checks (
    organization_id uuid NOT NULL REFERENCES organizations (id),
    catalog_connection_id uuid NOT NULL,
    capabilities jsonb NOT NULL,
    config_fingerprint text NOT NULL,
    checked_at timestamptz NOT NULL,
    PRIMARY KEY (organization_id, catalog_connection_id),
    FOREIGN KEY (organization_id, catalog_connection_id)
        REFERENCES catalog_connections (organization_id, id)
);
