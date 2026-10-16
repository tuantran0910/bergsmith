-- BS-003.T3: jobs, plans, tasks, attempts with leases, receipts, publications,
-- and job events.
--
-- State names match `as_str` in `bergsmith-common`. The invariants enforced
-- here hold whatever the application does:
--   * a job has at most one plan and one publication;
--   * a task has at most one active (Granted or Running) attempt;
--   * a task has at most one accepted receipt;
--   * plans, receipts, and job events are immutable.
-- Accepted results cannot enter two publications: a receipt belongs to one
-- task, a task to one job, and a job has one publication.

CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    table_identity_id uuid NOT NULL,
    state text NOT NULL CHECK (state IN (
        'Planning', 'Queued', 'Running', 'ReadyToCommit', 'Committing',
        'CommitOutcomeUnknown', 'Cancelling', 'Succeeded', 'NoOp', 'Cancelled',
        'ReplanRequired', 'Failed')),
    state_revision bigint NOT NULL DEFAULT 0 CHECK (state_revision >= 0),
    state_reason text,
    retry_of_job_id uuid,
    created_by uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id),
    FOREIGN KEY (organization_id, retry_of_job_id) REFERENCES jobs (organization_id, id),
    FOREIGN KEY (organization_id, created_by) REFERENCES principals (organization_id, id),
    CHECK (retry_of_job_id IS DISTINCT FROM id)
);

CREATE INDEX jobs_active_by_table ON jobs (table_identity_id)
    WHERE state NOT IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed');

-- The immutable execution plan. One per job.
CREATE TABLE plans (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL UNIQUE REFERENCES jobs (id),
    content jsonb NOT NULL,
    content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (job_id, id)
);

CREATE TRIGGER plans_are_immutable BEFORE UPDATE OR DELETE ON plans
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

CREATE TABLE tasks (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL,
    plan_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal >= 0),
    state text NOT NULL CHECK (state IN (
        'Pending', 'Leased', 'RetryWaiting', 'Accepted', 'Cancelled', 'Failed')),
    attempts_started integer NOT NULL DEFAULT 0 CHECK (attempts_started BETWEEN 0 AND 3),
    retry_not_before timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plan_id, ordinal),
    UNIQUE (job_id, id),
    FOREIGN KEY (job_id, plan_id) REFERENCES plans (job_id, id),
    CHECK ((state = 'RetryWaiting') = (retry_not_before IS NOT NULL))
);

-- One execution attempt. `generation` counts attempts per task from 1, and
-- the lease is decided with database time.
CREATE TABLE attempts (
    id uuid PRIMARY KEY,
    task_id uuid NOT NULL REFERENCES tasks (id),
    generation integer NOT NULL CHECK (generation BETWEEN 1 AND 3),
    state text NOT NULL CHECK (state IN (
        'Granted', 'Running', 'Accepted', 'Failed', 'Expired', 'Cancelled')),
    -- Worker sessions arrive in BS-013, which adds the foreign key.
    worker_session_id uuid NOT NULL,
    granted_at timestamptz NOT NULL DEFAULT now(),
    lease_expires_at timestamptz NOT NULL,
    finished_at timestamptz,
    UNIQUE (task_id, generation),
    UNIQUE (task_id, id),
    CHECK (lease_expires_at > granted_at),
    CHECK ((state IN ('Granted', 'Running')) = (finished_at IS NULL))
);

CREATE UNIQUE INDEX attempts_one_active_per_task ON attempts (task_id)
    WHERE state IN ('Granted', 'Running');

-- The immutable output receipt of an accepted attempt. One per task.
CREATE TABLE receipts (
    attempt_id uuid PRIMARY KEY,
    task_id uuid NOT NULL UNIQUE,
    content jsonb NOT NULL,
    content_hash text NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    accepted_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (task_id, attempt_id) REFERENCES attempts (task_id, id)
);

CREATE TRIGGER receipts_are_immutable BEFORE UPDATE OR DELETE ON receipts
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();

-- A job's single publication operation. BS-012 adds the commit intent.
CREATE TABLE publications (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL UNIQUE REFERENCES jobs (id),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- The append-only history of each job.
CREATE TABLE job_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs (id),
    kind text NOT NULL CHECK (kind IN (
        'Created', 'Transitioned', 'CancellationRequested', 'PublicationAuthorized')),
    from_state text,
    to_state text NOT NULL,
    state_revision bigint NOT NULL,
    reason text,
    actor_id uuid REFERENCES principals (id),
    occurred_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((kind = 'Transitioned') = (from_state IS NOT NULL AND from_state <> to_state))
);

CREATE INDEX job_events_by_job ON job_events (job_id, id);

CREATE TRIGGER job_events_are_immutable BEFORE UPDATE OR DELETE ON job_events
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
