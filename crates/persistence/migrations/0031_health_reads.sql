-- BS-028.T4: read health inputs by table.
--
-- The tables list evaluates every visible table's health from its newest
-- sample, its confirmation, and the thresholds of its enabled policies. Those
-- reads are keyed by a set of table ids, and schedules only had schedule and
-- policy indexes. The index is partial because a disabled schedule never
-- contributes a threshold set to a page, which is the only reader that needs
-- this key.
CREATE INDEX maintenance_schedules_by_table
    ON maintenance_schedules (organization_id, table_identity_id)
    WHERE enabled;
