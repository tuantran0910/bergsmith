-- BS-022.T4: the revision of one catalog name.
--
-- A name is removed and registered again — the table was dropped and
-- recreated, or a different table took the name — so "remove db.events" has to
-- say which row it observed. Every insert takes a fresh revision from this
-- sequence, and a removal compares it under the row lock, so a stale request
-- can never delete the replacement.
CREATE SEQUENCE table_alias_revision AS bigint;

ALTER TABLE table_aliases
    ADD COLUMN revision bigint NOT NULL DEFAULT nextval('table_alias_revision');
