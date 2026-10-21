-- BS-018.T3: per-job event sequence and change notification for SSE.
--
-- Every append to `job_events` runs under the job's row lock (or in the
-- transaction that inserts the job), so one job's events commit in append
-- order. `sequence` records that order as 1, 2, 3, ... per job and becomes the
-- public SSE event ID. The global identity `id` is not commit-ordered across
-- jobs and would disclose other organizations' event volume, so it is never
-- exposed.
--
-- The unique constraint fails closed: an append path that skipped the job
-- lock could compute a duplicate sequence, and that insert is rejected rather
-- than silently reordering a stream.

ALTER TABLE job_events ADD COLUMN sequence bigint;

-- Existing rows already commit in identity order per job. The immutability
-- trigger is disabled only for this one-time backfill, inside the migration's
-- transaction.
ALTER TABLE job_events DISABLE TRIGGER job_events_are_immutable;
UPDATE job_events e
SET sequence = ordered.sequence
FROM (
    SELECT id, row_number() OVER (PARTITION BY job_id ORDER BY id) AS sequence
    FROM job_events
) ordered
WHERE e.id = ordered.id;
ALTER TABLE job_events ENABLE TRIGGER job_events_are_immutable;

ALTER TABLE job_events
    ALTER COLUMN sequence SET NOT NULL,
    ADD CONSTRAINT job_events_sequence_positive CHECK (sequence > 0),
    ADD CONSTRAINT job_events_sequence_unique UNIQUE (job_id, sequence);

-- The unique index serves per-job reads in sequence order.
DROP INDEX job_events_by_job;

-- Wakes event streams after commit. A notification carries only the job ID;
-- readers always re-query the ledger, so a lost notification delays delivery
-- but never loses an event.
CREATE FUNCTION bergsmith_notify_job_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('bergsmith_job_events', NEW.job_id::text);
    RETURN NULL;
END;
$$;

CREATE TRIGGER job_events_notify AFTER INSERT ON job_events
    FOR EACH ROW EXECUTE FUNCTION bergsmith_notify_job_event();
