-- BS-003.T5: cancellation requests and the send-authorization boundary.
--
-- `publication_started_at` is set by the send-authorization transaction
-- immediately before the first outbound commit request. It is the
-- cancellation boundary: once set, cancellation can no longer prevent
-- publication. Cancellation and send authorization serialize on the job row.

ALTER TABLE jobs
    ADD COLUMN cancellation_requested_at timestamptz,
    ADD COLUMN cancellation_requested_by uuid,
    ADD COLUMN cancellation_reason text,
    ADD COLUMN publication_started_at timestamptz,
    ADD FOREIGN KEY (organization_id, cancellation_requested_by)
        REFERENCES principals (organization_id, id),
    ADD CONSTRAINT jobs_cancellation_fields_need_a_request CHECK (
        cancellation_requested_at IS NOT NULL
        OR (cancellation_requested_by IS NULL AND cancellation_reason IS NULL)),
    -- A job that may have sent a commit request is never cancelling, and only
    -- jobs that reached Committing can have authorized a send.
    ADD CONSTRAINT jobs_publication_start_matches_state CHECK (
        publication_started_at IS NULL
        OR state IN ('Committing', 'CommitOutcomeUnknown', 'Succeeded', 'Cancelled',
                     'ReplanRequired', 'Failed'));

-- Cancellation requests and send authorization are monotonic: once recorded
-- they never change or disappear.
CREATE FUNCTION bergsmith_jobs_keep_boundaries() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.publication_started_at IS NOT NULL
       AND NEW.publication_started_at IS DISTINCT FROM OLD.publication_started_at THEN
        RAISE EXCEPTION 'job % publication_started_at is immutable once set', OLD.id
            USING ERRCODE = 'restrict_violation';
    END IF;
    IF OLD.cancellation_requested_at IS NOT NULL
       AND (NEW.cancellation_requested_at IS DISTINCT FROM OLD.cancellation_requested_at
            OR NEW.cancellation_requested_by IS DISTINCT FROM OLD.cancellation_requested_by
            OR NEW.cancellation_reason IS DISTINCT FROM OLD.cancellation_reason) THEN
        RAISE EXCEPTION 'job % cancellation request is immutable once recorded', OLD.id
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER jobs_keep_boundaries BEFORE UPDATE ON jobs
    FOR EACH ROW EXECUTE FUNCTION bergsmith_jobs_keep_boundaries();
