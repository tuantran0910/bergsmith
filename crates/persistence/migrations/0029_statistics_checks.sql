-- Two sample counters and the freshness ledger for the health model
-- (BS-028.T1, ADR 0059).
--
-- `data_manifest_count` is the number of manifests that carry data files, the
-- denominator of manifest fragmentation; delete manifests cannot hold data
-- files, so counting them overstated fragmentation.
--
-- `max_partition_files` is the busiest partition's data-file count,
-- independent of `statistics.sample.top_partitions`, which bounds only what a
-- page displays. A zero display bound must not hide skew.
--
-- `table_statistics_checks` records when a pass last read a table's metadata
-- successfully. The sample itself is append-only and unchanged metadata
-- records nothing (ADR 0057), so the sample's age is not the table's freshness:
-- an unchanged table is confirmed fresh on every pass without a new sample.
--
-- `sample_version` names which counters a sample carries. Version 1 adds the
-- two columns below; 0 is a sample written before them. A legacy sample cannot
-- be upgraded in place (samples are immutable), so the collector replaces an
-- incomplete sample with a new observation instead of taking the unchanged
-- shortcut, which would otherwise leave the missing counters at zero forever.
ALTER TABLE table_statistics
    ADD COLUMN sample_version smallint NOT NULL DEFAULT 0,
    ADD COLUMN data_manifest_count bigint NOT NULL DEFAULT 0,
    ADD COLUMN max_partition_files bigint NOT NULL DEFAULT 0;

CREATE TABLE table_statistics_checks (
    organization_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    -- The newest successful confirmation. Upserts only move this forward.
    checked_at timestamptz NOT NULL,
    PRIMARY KEY (organization_id, table_identity_id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id)
);
