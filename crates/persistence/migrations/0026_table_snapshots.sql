-- BS-026.T1: the snapshot ledger behind the table explorer timeline.
--
-- One row per committed snapshot, deduplicated by snapshot ID, so a snapshot
-- is recorded once however often the collector sees it. The row keeps the
-- committing engine's summary properties verbatim: they carry its change
-- counters and, on a Bergsmith publication, the job, publication, and
-- proposal evidence that links the commit to its job.
--
-- Nothing is rewritten. Retention prunes by when the row was observed, which
-- is how long Bergsmith keeps knowledge of a commit, not how long the
-- catalog's metadata retains the snapshot itself.

CREATE TABLE table_snapshots (
    organization_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    catalog_connection_id uuid NOT NULL,
    snapshot_id bigint NOT NULL,
    parent_snapshot_id bigint,
    sequence_number bigint NOT NULL,
    committed_at timestamptz NOT NULL,
    operation text NOT NULL CHECK (length(operation) > 0),
    summary jsonb NOT NULL CHECK (jsonb_typeof(summary) = 'object'),
    observed_at timestamptz NOT NULL,
    PRIMARY KEY (organization_id, table_identity_id, snapshot_id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id),
    FOREIGN KEY (organization_id, catalog_connection_id)
        REFERENCES catalog_connections (organization_id, id)
);

-- The timeline reads one table's snapshots newest first; the snapshot ID is
-- the tiebreak for commits recorded in the same instant.
CREATE INDEX table_snapshots_by_commit
    ON table_snapshots (organization_id, table_identity_id, committed_at DESC, snapshot_id DESC);

-- Retention prunes the oldest observations across every table.
CREATE INDEX table_snapshots_by_observed_at ON table_snapshots (observed_at);
