-- Filtered lists (BS-033.T4, ADR 0067).
--
-- Lists page by identifier within an organization. Each index below gives
-- one filter an index range in that order, so a filtered page reads only
-- its matching rows in either direction. Facet counts use the same indexes.
--
-- Name search (`q`) adds no index: it matches catalog, table, and principal
-- display names, which live in inventory-sized tables, never in table rows.

CREATE INDEX jobs_by_organization_state ON jobs (organization_id, state, id);
CREATE INDEX jobs_by_organization_operation ON jobs (organization_id, operation, id);
CREATE INDEX jobs_by_organization_table ON jobs (organization_id, table_identity_id, id);
CREATE INDEX jobs_by_organization_requester ON jobs (organization_id, created_by, id);

-- The audit log is append-only, so these indexes cost only the insert path.
CREATE INDEX audit_events_by_organization_outcome
    ON audit_events (organization_id, outcome, id);
CREATE INDEX audit_events_by_organization_action
    ON audit_events (organization_id, action, id);
CREATE INDEX audit_events_by_organization_actor
    ON audit_events (organization_id, actor_principal_id, id);
