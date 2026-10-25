-- BS-025.T2: metadata-only table samples with a bounded history.
--
-- One row per observation of a table's current state. The newest row is the
-- table's current statistics; older rows are the history the trends are drawn
-- from. Rows are append-only except for retention pruning by `sampled_at`, so
-- a sample is never "refreshed" in place.
--
-- A sample is keyed by when it was taken, not by the metadata location: the
-- collector records nothing when that location is unchanged, because the
-- counters would be identical.

CREATE TABLE table_statistics (
    organization_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    catalog_connection_id uuid NOT NULL,
    sampled_at timestamptz NOT NULL,
    metadata_location text NOT NULL CHECK (length(metadata_location) > 0),
    snapshot_id bigint,
    last_operation text CHECK (last_operation IS NULL OR length(last_operation) > 0),
    record_count bigint NOT NULL CHECK (record_count >= 0),
    data_files bigint NOT NULL CHECK (data_files >= 0),
    data_bytes bigint NOT NULL CHECK (data_bytes >= 0),
    delete_files bigint NOT NULL CHECK (delete_files >= 0),
    delete_bytes bigint NOT NULL CHECK (delete_bytes >= 0),
    position_delete_files bigint NOT NULL CHECK (position_delete_files >= 0),
    equality_delete_files bigint NOT NULL CHECK (equality_delete_files >= 0),
    position_delete_records bigint NOT NULL CHECK (position_delete_records >= 0),
    equality_delete_records bigint NOT NULL CHECK (equality_delete_records >= 0),
    manifest_count bigint NOT NULL CHECK (manifest_count >= 0),
    manifest_bytes bigint NOT NULL CHECK (manifest_bytes >= 0),
    partition_count bigint NOT NULL CHECK (partition_count >= 0),
    small_files bigint NOT NULL CHECK (small_files >= 0),
    target_file_size_bytes bigint NOT NULL CHECK (target_file_size_bytes >= 0),
    snapshot_count bigint NOT NULL CHECK (snapshot_count >= 0),
    oldest_snapshot_at timestamptz,
    newest_snapshot_at timestamptz,
    size_histogram jsonb NOT NULL CHECK (jsonb_typeof(size_histogram) = 'array'),
    top_partitions jsonb NOT NULL CHECK (jsonb_typeof(top_partitions) = 'array'),
    PRIMARY KEY (organization_id, table_identity_id, sampled_at),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id),
    FOREIGN KEY (organization_id, catalog_connection_id)
        REFERENCES catalog_connections (organization_id, id)
);

-- Retention prunes the oldest samples first, across every table.
CREATE INDEX table_statistics_by_sampled_at ON table_statistics (sampled_at);
