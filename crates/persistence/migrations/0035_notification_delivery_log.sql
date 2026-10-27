-- BS-029.T4: the delivery log pages newest first per organization.
--
-- The keyset predicate and order both use (created_at, id), so this index
-- bounds each page instead of scanning and sorting the retained history.
CREATE INDEX notification_deliveries_log
    ON notification_deliveries (organization_id, created_at DESC, id DESC);
