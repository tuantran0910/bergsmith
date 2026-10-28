-- Latest complete storage observation and independent check status (ADR 0064).
CREATE TABLE table_storage (
    organization_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    sampled_at timestamptz,
    metadata_location text,
    snapshot_id bigint,
    content jsonb,
    checked_at timestamptz NOT NULL,
    failure_reason text CHECK (failure_reason ~ '^[A-Za-z][A-Za-z0-9]{0,79}$'),
    PRIMARY KEY (organization_id, table_identity_id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id) ON DELETE CASCADE,
    CHECK ((sampled_at IS NULL AND content IS NULL AND metadata_location IS NULL)
        OR (sampled_at IS NOT NULL AND content IS NOT NULL AND metadata_location IS NOT NULL))
);
CREATE INDEX table_storage_due ON table_storage (checked_at, organization_id, table_identity_id);
