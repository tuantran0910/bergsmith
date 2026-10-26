-- Terminal-job scans for maintenance impact (BS-027.T2).
--
-- The history lists one table's newest terminal jobs, and the organization
-- totals scan terminal jobs across every table the caller may view. Terminal
-- states are immutable, so both indexes stay small next to the jobs table and
-- need no maintenance beyond the ordinary insert path.
CREATE INDEX jobs_terminal_by_table
    ON jobs (organization_id, table_identity_id, id DESC)
    WHERE state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed');

CREATE INDEX jobs_terminal_by_organization
    ON jobs (organization_id, id DESC)
    WHERE state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed');

-- The landing overview windows terminal jobs by their terminal transition,
-- not by creation, so a job that started earlier and ended inside the window
-- still counts.
CREATE INDEX jobs_terminal_by_organization_ended
    ON jobs (organization_id, updated_at DESC)
    WHERE state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed');
