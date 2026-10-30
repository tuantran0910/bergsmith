//! SQL fragments shared by the resource reads and the filtered lists.
//!
//! Every fragment is a compile-time literal. The access predicates read the
//! caller's organization, principal, and granting roles from the parameters
//! `$1`, `$2`, and `$3`, so a query that uses them binds those three first.
//! Filtering precedes `ORDER BY` / `LIMIT`, so inaccessible rows never consume
//! a page and never reach a count.

/// Whether the caller may see table `t`.
macro_rules! table_access {
    () => {
        "EXISTS (
    SELECT 1 FROM principal_role_grants g
    WHERE g.organization_id = $1 AND g.principal_id = $2
      AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
      AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
        OR g.table_identity_id = t.id
        OR EXISTS (SELECT 1 FROM table_aliases a
                   WHERE a.organization_id = $1 AND a.table_identity_id = t.id
                     AND a.catalog_connection_id = g.catalog_connection_id)))"
    };
}

/// Whether the caller may see catalog connection `r`.
macro_rules! catalog_access {
    () => {
        "EXISTS (
    SELECT 1 FROM principal_role_grants g
    WHERE g.organization_id = $1 AND g.principal_id = $2
      AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
      AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
        OR g.catalog_connection_id = r.id))"
    };
}

/// Whether the caller may see alias `$alias`: an organization or table grant
/// reveals every alias of its table, a connection grant only that
/// connection's.
macro_rules! alias_visible {
    ($alias:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM principal_role_grants g
                WHERE g.organization_id = $1 AND g.principal_id = $2
                  AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                  AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
                    OR g.table_identity_id = ",
            $alias,
            ".table_identity_id
                    OR g.catalog_connection_id = ",
            $alias,
            ".catalog_connection_id))"
        )
    };
}

/// The aliases of table `$table` the caller may see.
macro_rules! visible_aliases {
    ($table:literal) => {
        concat!(
            "COALESCE((SELECT jsonb_agg(jsonb_build_object(
              'catalog_id', a.catalog_connection_id, 'namespace', a.namespace,
              'name', a.table_name, 'revision', a.revision)
              ORDER BY a.catalog_connection_id, a.namespace, a.table_name)
            FROM table_aliases a WHERE a.organization_id = $1 AND a.table_identity_id = ",
            $table,
            " AND ",
            alias_visible!("a"),
            "), '[]'::jsonb)"
        )
    };
}

/// A catalog connection `r` as lists and the summary read carry it.
macro_rules! catalog_summary {
    () => {
        "jsonb_build_object('id', r.id, 'name', r.name, 'enabled', r.enabled, 'settings', jsonb_build_object('maintenance_enabled', r.maintenance_enabled, 'max_execution_seconds', r.max_execution_seconds, 'max_unresolved_publication_seconds', r.max_unresolved_publication_seconds), 'revision', r.revision, 'updated_at', r.updated_at, 'created_at', r.created_at)"
    };
}

/// A table identity `r` with its visible aliases and newest sample.
macro_rules! table_summary {
    () => {
        concat!(
            "jsonb_build_object('id', r.id, 'table_uuid', r.table_uuid, 'created_at', r.created_at,
          'aliases', ",
            visible_aliases!("r.id"),
            ", 'statistics', (SELECT jsonb_build_object(
                'sampled_at', s.sampled_at,
                'record_count', s.record_count::text,
                'data_files', s.data_files::text,
                'data_bytes', s.data_bytes::text,
                'delete_files', s.delete_files::text,
                'small_files', s.small_files::text)
              FROM table_statistics s
              WHERE s.organization_id = r.organization_id AND s.table_identity_id = r.id
              ORDER BY s.sampled_at DESC LIMIT 1))"
        )
    };
}

/// A job `r` as the list carries it.
///
/// Names are for display; `table_id` and `created_by` stay the keys. The
/// requester's foreign key names a principal of the job's organization, and
/// display names are required, so `created_by_name` is never null.
macro_rules! job_summary {
    () => {
        concat!(
            "jsonb_build_object('id', r.id, 'table_id', r.table_identity_id, 'table_aliases', ",
            visible_aliases!("r.table_identity_id"),
            ", 'operation', CASE r.operation WHEN 'ExpireSnapshots' THEN 'expire_snapshots' WHEN 'RemoveOrphanFiles' THEN 'remove_orphan_files' WHEN 'RewriteManifests' THEN 'rewrite_manifests' WHEN 'RemoveDanglingDeletes' THEN 'remove_dangling_deletes' ELSE 'compact' END, 'state', r.state, 'revision', r.state_revision, 'state_reason', r.state_reason, \
             'retry_of_job_id', r.retry_of_job_id, 'created_by', r.created_by, \
             'created_by_name', (SELECT p.display_name FROM principals p \
                WHERE p.id = r.created_by AND p.organization_id = r.organization_id), \
             'created_at', r.created_at, 'updated_at', r.updated_at, \
             'cancellation_requested', r.cancellation_requested_at IS NOT NULL, \
             'publication_started', r.publication_started_at IS NOT NULL, \
             'trigger', CASE WHEN r.trigger_finding IS NULL THEN NULL \
                ELSE jsonb_build_object('kind', r.trigger_finding, \
                                        'severity', r.trigger_severity) END)"
        )
    };
}
