-- BS-019.T3: schedule windows and per-run input budgets.
--
-- A schedule may name a daily local-time window in its own time zone. The
-- window gates the start of an admitted run: an occurrence whose evaluation
-- happens outside the window is recorded as `Skipped` with reason
-- `OutsideWindow`, and the cursor advances. `window_start_minute` is inclusive,
-- `window_end_minute` exclusive, and a start later than the end wraps past
-- midnight. The coordinator validates the pair before insert; the checks here
-- keep the two columns consistent and bound them to one day.
--
-- A policy names the most input bytes one run may rewrite. The planner caps
-- the job and group limits by it, so a scheduled run stays inside the table's
-- commit cadence. Existing policies receive the planner's default maximum
-- (100 GiB), which preserves their previous behavior.

-- The default matches PolicySpec::MAX_RUN_INPUT_BYTES; keep them in step.
ALTER TABLE maintenance_policies
    ADD COLUMN max_input_bytes bigint NOT NULL DEFAULT 107374182400;

ALTER TABLE maintenance_policies
    ADD CONSTRAINT maintenance_policies_max_input_bytes_range
    CHECK (max_input_bytes BETWEEN 1048576 AND 107374182400);

-- A run that cannot hold one replacement file would always plan nothing.
ALTER TABLE maintenance_policies
    ADD CONSTRAINT maintenance_policies_max_input_covers_target
    CHECK (max_input_bytes >= target_file_size_bytes);

ALTER TABLE maintenance_schedules
    ADD COLUMN window_start_minute integer,
    ADD COLUMN window_end_minute integer;

-- Both bounds are set or both are null, and a set pair describes a non-empty
-- interval within one day. A start equal to the end is ambiguous between "no
-- time" and "all day"; use a null window for "any time".
ALTER TABLE maintenance_schedules
    ADD CONSTRAINT maintenance_schedules_window_bounds
    CHECK (
        (window_start_minute IS NULL) = (window_end_minute IS NULL)
        AND (
            window_start_minute IS NULL
            OR (
                window_start_minute BETWEEN 0 AND 1439
                AND window_end_minute BETWEEN 0 AND 1439
                AND window_start_minute <> window_end_minute
            )
        )
    );
