-- BS-031.T2: orphan-file cleanup is a coordinator job with an immutable plan
-- and one receipt per planned object (ADR 0062).
--
-- `RemoveOrphanFiles` never publishes, so its job enters `Deleting` from
-- `ReadyToCommit`, records a disposition per candidate, and only then moves to
-- `Succeeded`. The state is not listed by the terminal scans, so it counts as
-- active until the receipts are complete.

ALTER TABLE jobs DROP CONSTRAINT jobs_state_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_state_check CHECK (state IN (
    'Planning', 'Queued', 'Running', 'ReadyToCommit', 'Committing',
    'CommitOutcomeUnknown', 'Deleting', 'Cancelling', 'Succeeded', 'NoOp',
    'Cancelled', 'ReplanRequired', 'Failed'));

ALTER TABLE jobs DROP CONSTRAINT jobs_operation_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_operation_check CHECK (
    operation IN ('Compact', 'ExpireSnapshots', 'RemoveOrphanFiles'));

CREATE TABLE orphan_plans (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL UNIQUE REFERENCES jobs (id) ON DELETE CASCADE,
    content jsonb NOT NULL,
    content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER orphan_plans_are_immutable BEFORE UPDATE OR DELETE ON orphan_plans
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

-- `Deleted` means the object was removed; `Skipped` means re-verification kept
-- it (it became referenced, young, or owned by Bergsmith); `Absent` means the
-- object was already gone when the batch looked, so an earlier attempt may
-- have removed it, but this receipt cannot prove that it did. Every row
-- resolves its candidate, so the job finishes once the plan has one row per
-- object. A retry after a crash between the delete and the insert collides on
-- the primary key and is ignored: deleting a missing object is a no-op.
CREATE TABLE orphan_deletions (
    job_id uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    path text NOT NULL CHECK (length(path) > 0),
    disposition text NOT NULL CHECK (disposition IN ('Deleted', 'Skipped', 'Absent')),
    size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (job_id, path)
);

CREATE TRIGGER orphan_deletions_are_immutable BEFORE UPDATE OR DELETE ON orphan_deletions
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
