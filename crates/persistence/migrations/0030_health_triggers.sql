-- Health triggers and per-policy thresholds (BS-028.T3, ADR 0059).
--
-- A policy may override the health thresholds used to judge a table and may
-- name one finding with the severity that admits maintenance. An empty object
-- keeps every documented default; a null trigger keeps schedule admission
-- unconditional.
ALTER TABLE maintenance_policies
    ADD COLUMN health_thresholds jsonb NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN health_trigger jsonb;

-- A job admitted because of a health condition records the condition, so the
-- job explains its own cause. Both columns are set together.
ALTER TABLE jobs
    ADD COLUMN trigger_finding text,
    ADD COLUMN trigger_severity text;

ALTER TABLE jobs
    ADD CONSTRAINT jobs_trigger_names_both
    CHECK ((trigger_finding IS NULL) = (trigger_severity IS NULL));
