-- BS-031.T1: snapshot expiration is a maintenance operation with its own
-- immutable, metadata-only plan.
--
-- Every existing job is a compaction job, so `operation` defaults to
-- 'Compact'. An expiration plan never changes once written, like `plans`.
ALTER TABLE jobs ADD COLUMN operation text NOT NULL DEFAULT 'Compact'
    CHECK (operation IN ('Compact', 'ExpireSnapshots'));

CREATE TABLE expiration_plans (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL UNIQUE REFERENCES jobs (id) ON DELETE CASCADE,
    content jsonb NOT NULL,
    content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER expiration_plans_are_immutable BEFORE UPDATE OR DELETE ON expiration_plans
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
