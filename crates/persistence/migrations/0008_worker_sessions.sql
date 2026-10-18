-- BS-013: authenticated sessions and immutable poll-to-attempt associations.
CREATE TABLE worker_sessions (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL,
    principal_id uuid NOT NULL,
    pool text NOT NULL CHECK (pool ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    worker_name text NOT NULL CHECK (length(worker_name) BETWEEN 1 AND 200),
    capabilities text[] NOT NULL,
    memory_bytes bigint NOT NULL CHECK (memory_bytes > 0),
    cpu_millis integer NOT NULL CHECK (cpu_millis > 0),
    build_version text NOT NULL CHECK (length(build_version) BETWEEN 1 AND 200),
    draining boolean NOT NULL DEFAULT false,
    superseded boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    lease_expires_at timestamptz NOT NULL,
    FOREIGN KEY (organization_id, principal_id) REFERENCES principals (organization_id, id)
);
CREATE UNIQUE INDEX worker_sessions_one_current_name
    ON worker_sessions (organization_id, principal_id, pool, worker_name) WHERE NOT superseded;

-- Preserve old ledger rows without granting legacy workers new authority.
INSERT INTO principals (id, organization_id, kind, issuer, subject, display_name)
SELECT gen_random_uuid(), organization_id, 'Service', 'bergsmith:legacy-worker',
       'legacy', 'legacy worker (fenced)'
FROM jobs GROUP BY organization_id;
INSERT INTO worker_sessions (id, organization_id, principal_id, pool, worker_name,
    capabilities, memory_bytes, cpu_millis, build_version, draining, superseded, lease_expires_at)
SELECT DISTINCT ON (a.worker_session_id) a.worker_session_id, j.organization_id, p.id,
    'legacy', 'legacy', ARRAY[]::text[], 1, 1, 'legacy', true, true, clock_timestamp()
FROM attempts a JOIN tasks t ON t.id = a.task_id JOIN jobs j ON j.id = t.job_id
JOIN principals p ON p.organization_id = j.organization_id AND p.issuer = 'bergsmith:legacy-worker';
ALTER TABLE attempts ADD CONSTRAINT attempts_worker_session_fk
    FOREIGN KEY (worker_session_id) REFERENCES worker_sessions (id);

CREATE TABLE worker_polls (
    worker_session_id uuid NOT NULL REFERENCES worker_sessions (id),
    request_id uuid NOT NULL,
    attempt_id uuid UNIQUE REFERENCES attempts (id),
    -- Safe grant fields only: storage credentials are never persisted.
    grant_bytes bytea,
    PRIMARY KEY (worker_session_id, request_id),
    CHECK ((attempt_id IS NULL) = (grant_bytes IS NULL))
);
CREATE TRIGGER worker_polls_are_immutable BEFORE UPDATE OR DELETE ON worker_polls
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

CREATE TABLE attempt_failures (
    attempt_id uuid PRIMARY KEY REFERENCES attempts (id),
    reason_code text NOT NULL,
    disposition text NOT NULL,
    job_state text NOT NULL
);
CREATE TRIGGER attempt_failures_are_immutable BEFORE UPDATE OR DELETE ON attempt_failures
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
