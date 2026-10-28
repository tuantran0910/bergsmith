-- BS-031.T3: manifest rewrite and dangling-delete removal are metadata-only
-- operations with one immutable plan table (ADR 0063).
--
-- Both publish a `main` snapshot through the ordinary publication intent, so
-- they need no new job state. A plan never changes once written, like `plans`.

ALTER TABLE jobs DROP CONSTRAINT jobs_operation_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_operation_check CHECK (
    operation IN ('Compact', 'ExpireSnapshots', 'RemoveOrphanFiles',
                  'RewriteManifests', 'RemoveDanglingDeletes'));

CREATE TABLE metadata_plans (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL UNIQUE REFERENCES jobs (id) ON DELETE CASCADE,
    content jsonb NOT NULL,
    content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER metadata_plans_are_immutable BEFORE UPDATE OR DELETE ON metadata_plans
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
