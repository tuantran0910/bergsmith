-- BS-022.T3: the execution budget of one job.
--
-- The deadline is written once, with the job's plan, and bounds the whole
-- job: every attempt and every retry must fit before it. A job without a
-- deadline predates the budget and is not bounded.
ALTER TABLE jobs ADD COLUMN deadline_at timestamptz;
