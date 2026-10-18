-- BS-012: immutable intent, canonical-table barrier, and retained output inventory.
CREATE TABLE publication_intents (
    job_id uuid PRIMARY KEY REFERENCES jobs (id),
    publication_id uuid NOT NULL UNIQUE REFERENCES publications (id),
    content jsonb NOT NULL,
    request_body bytea NOT NULL,
    request_hash text NOT NULL CHECK (request_hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER publication_intents_are_immutable BEFORE UPDATE OR DELETE ON publication_intents
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

CREATE TABLE publication_barriers (
    table_identity_id uuid PRIMARY KEY REFERENCES table_identities (id),
    job_id uuid NOT NULL UNIQUE REFERENCES publication_intents (job_id)
);

-- Prefix registration precedes execution. Finalized inventory commits with receipt acceptance.
CREATE TABLE attempt_output_prefixes (
    attempt_id uuid PRIMARY KEY REFERENCES attempts (id),
    prefix text NOT NULL UNIQUE CHECK (length(prefix) > 0),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER attempt_output_prefixes_are_immutable BEFORE UPDATE OR DELETE ON attempt_output_prefixes
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

CREATE TABLE attempt_output_objects (
    attempt_id uuid NOT NULL REFERENCES attempt_output_prefixes (attempt_id),
    path text PRIMARY KEY,
    size_bytes bigint NOT NULL CHECK (size_bytes > 0),
    FOREIGN KEY (attempt_id) REFERENCES receipts (attempt_id)
);
CREATE TRIGGER attempt_output_objects_are_immutable BEFORE UPDATE OR DELETE ON attempt_output_objects
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

-- Pending terminal disposition is separate from the immutable request.
CREATE TABLE publication_pending_dispositions (
    job_id uuid PRIMARY KEY REFERENCES publication_intents (job_id),
    state text NOT NULL CHECK (state IN ('Cancelled', 'Failed', 'ReplanRequired')),
    reason text NOT NULL
);
