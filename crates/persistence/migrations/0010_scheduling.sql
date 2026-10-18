-- Safe immutable dispatch configuration; never storage or catalog credentials.
CREATE TABLE dispatch_specs (
    task_id uuid PRIMARY KEY REFERENCES tasks (id),
    pool text NOT NULL CHECK (pool ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    capability text NOT NULL CHECK (length(capability) BETWEEN 1 AND 200),
    artifact_uri text NOT NULL CHECK (length(artifact_uri) > 0),
    artifact_sha256 bytea NOT NULL CHECK (octet_length(artifact_sha256) = 32),
    envelope_bytes bytea NOT NULL CHECK (octet_length(envelope_bytes) > 0),
    memory_bytes bigint NOT NULL CHECK (memory_bytes > 0),
    cpu_millis integer NOT NULL CHECK (cpu_millis > 0)
);
CREATE TRIGGER dispatch_specs_are_immutable BEFORE UPDATE OR DELETE ON dispatch_specs
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

CREATE TABLE job_retry_policies (
    job_id uuid PRIMARY KEY REFERENCES jobs (id),
    base_millis bigint NOT NULL CHECK (base_millis > 0),
    max_millis bigint NOT NULL CHECK (max_millis >= base_millis)
);
CREATE TRIGGER job_retry_policies_are_immutable BEFORE UPDATE OR DELETE ON job_retry_policies
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

-- Serialize admission and grant budget decisions; never held across network I/O.
CREATE TABLE scheduler_guard (singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton));
INSERT INTO scheduler_guard DEFAULT VALUES;
CREATE TABLE execution_admissions (
    job_id uuid PRIMARY KEY REFERENCES jobs (id),
    table_identity_id uuid NOT NULL REFERENCES table_identities (id),
    pool text NOT NULL,
    released boolean NOT NULL DEFAULT false
);
CREATE UNIQUE INDEX one_execution_owner_per_table
    ON execution_admissions (table_identity_id) WHERE NOT released;
CREATE INDEX tasks_retry_deadline ON tasks (retry_not_before, id) WHERE state = 'RetryWaiting';
CREATE INDEX tasks_pending_by_id ON tasks (id) WHERE state = 'Pending';
CREATE INDEX jobs_reconciliation_by_id ON jobs (id)
    WHERE state NOT IN ('Succeeded','NoOp','Cancelled','ReplanRequired','Failed');
