-- BS-023.T1: a principal can be deactivated without losing its grants or its
-- history. Deactivation ends sessions, refuses sign-in, and stops future
-- admission and publication; it never deletes the row or cancels work.
ALTER TABLE principals ADD COLUMN deactivated_at timestamptz;

-- Listing active principals is the common administration query.
CREATE INDEX principals_active_by_organization
    ON principals (organization_id)
    WHERE deactivated_at IS NULL;
