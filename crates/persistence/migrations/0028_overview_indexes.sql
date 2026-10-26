-- The landing overview windows terminal jobs by their terminal transition,
-- not by creation, so a job that started earlier and ended inside the window
-- still counts (BS-027.T3, BS-032.T7).
--
-- Migration 0027 already merged, so this cannot extend its indexes: an
-- applied migration must keep its checksum.
CREATE INDEX jobs_terminal_by_organization_ended
    ON jobs (organization_id, updated_at DESC)
    WHERE state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed');
