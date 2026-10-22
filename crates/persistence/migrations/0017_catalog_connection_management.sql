-- BS-022.T1: revision-checked connection management. Existing connections stay usable.
ALTER TABLE catalog_connections
    ADD COLUMN enabled boolean NOT NULL DEFAULT true,
    ADD COLUMN revision bigint NOT NULL DEFAULT 0 CHECK (revision >= 0),
    ADD COLUMN updated_at timestamptz NOT NULL DEFAULT now();
