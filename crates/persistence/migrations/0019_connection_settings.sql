-- BS-022.T3: operating bounds one connection declares.
--
-- Existing connections keep working: they start with the default execution
-- budget, the default unresolved-publication age, and maintenance allowed.
ALTER TABLE catalog_connections
    ADD COLUMN max_execution_seconds bigint NOT NULL DEFAULT 180
        CHECK (max_execution_seconds > 0),
    ADD COLUMN max_unresolved_publication_seconds bigint NOT NULL DEFAULT 3600
        CHECK (max_unresolved_publication_seconds > 0),
    ADD COLUMN maintenance_enabled boolean NOT NULL DEFAULT true;
