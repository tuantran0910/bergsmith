-- BS-019.T2: schedule occurrence identity, deduplication, and coalescing.
--
-- `maintenance_schedule_state` holds each enabled schedule's next due instant,
-- so the scheduler can tell what is due without scanning history and can
-- advance past downtime or a disabled period without inventing occurrences.
-- The row is deleted while a schedule is disabled and recreated from "now" when
-- it is enabled again, so re-enabling never backfills the disabled interval.
-- Rows persisted before this migration are armed by the coordinator's first
-- pass (`occurrences::arm_missing`); cron instants are not computed in SQL.
--
-- `maintenance_schedule_occurrences` is the durable identity of one pending
-- evaluation. `scheduled_for` is the first due instant it represents; when the
-- coordinator was down for several occurrences, `coalesced_until` names the
-- last one folded into the same evaluation. The unique constraint on
-- `(organization_id, schedule_id, scheduled_for)` is what makes admission
-- idempotent: a replayed evaluation cannot admit a second job.
--
-- `job_id` is set for an admitted occurrence (the job that was created) and
-- for a coalesced one (the nonterminal job on the table that already owns the
-- work). A skipped occurrence has no job and carries a stable reason. The
-- partial unique index keeps one admitted occurrence per job, which the
-- publication gate relies on when it rechecks the policy.

CREATE TABLE maintenance_schedule_state (
    organization_id uuid NOT NULL,
    schedule_id uuid NOT NULL,
    -- NULL means the expression names no reachable instant; it stays dormant.
    next_due_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (organization_id, schedule_id),
    FOREIGN KEY (organization_id, schedule_id)
        REFERENCES maintenance_schedules (organization_id, id)
);

-- Due scans read only schedules with a reachable next instant, oldest first.
CREATE INDEX maintenance_schedule_state_due
    ON maintenance_schedule_state (next_due_at, schedule_id)
    WHERE next_due_at IS NOT NULL;

CREATE TABLE maintenance_schedule_occurrences (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL,
    schedule_id uuid NOT NULL,
    -- First due instant this evaluation represents.
    scheduled_for timestamptz NOT NULL,
    -- Latest due instant folded into this evaluation during downtime, if any.
    coalesced_until timestamptz,
    state text NOT NULL CHECK (state IN ('Pending', 'Admitted', 'Coalesced', 'Skipped')),
    job_id uuid,
    -- Stable reason for `Coalesced` and `Skipped`; NULL when admitted.
    reason text,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (organization_id, schedule_id, scheduled_for),
    FOREIGN KEY (organization_id, schedule_id)
        REFERENCES maintenance_schedules (organization_id, id),
    FOREIGN KEY (job_id) REFERENCES jobs (id),
    CHECK (coalesced_until IS NULL OR coalesced_until > scheduled_for),
    CHECK (state <> 'Admitted' OR (job_id IS NOT NULL AND reason IS NULL)),
    CHECK (state <> 'Coalesced' OR (job_id IS NOT NULL AND reason IS NOT NULL)),
    CHECK (state <> 'Skipped' OR (job_id IS NULL AND reason IS NOT NULL))
);

-- One admitted occurrence backs at most one job; coalesced rows may share the
-- blocking job, so the uniqueness applies only to admitted occurrences.
CREATE UNIQUE INDEX maintenance_schedule_occurrences_admitted_job
    ON maintenance_schedule_occurrences (job_id)
    WHERE state = 'Admitted';
