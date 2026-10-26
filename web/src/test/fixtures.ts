// Contract-typed fixtures. A change to the OpenAPI document that these no
// longer satisfy fails the typecheck, not just a test.

import type { Job, JobEvent, JobState, Schemas } from "../api/client";
import type {
    GrantScope,
    Permission,
    SessionInfo,
    SessionPermission,
} from "../api/auth";
import { schema } from "./fakes";

export const TABLE_ID = "0190f3c4-0000-7000-8000-000000000001";
export const CATALOG_ID = "0190f3c4-0000-7000-8000-0000000000c1";
export const JOB_ID = "0190f3c4-0000-7000-8000-00000000000a";
export const RETRY_JOB_ID = "0190f3c4-0000-7000-8000-00000000000b";
export const PRINCIPAL_ID = "0190f3c4-0000-7000-8000-0000000000aa";
export const POLICY_ID = "0190f3c4-0000-7000-8000-0000000000b1";
export const SCHEDULE_ID = "0190f3c4-0000-7000-8000-0000000000d1";
export const OCCURRENCE_ID = "0190f3c4-0000-7000-8000-0000000000d2";
export const SERVICE_ID = "0190f3c4-0000-7000-8000-0000000000d3";

const ORGANIZATION: GrantScope = { catalog_connection: null, table: null };

// Mirrors the coordinator's role-to-permission table (ADR 0032). A session
// fixture must carry the permissions the server would send for that role and
// scope, or the browser tests would exercise an impossible session.
const ROLE_PERMISSIONS: Record<string, readonly Permission[]> = {
    Viewer: ["inventory:read", "jobs:read", "artifacts:read"],
    Operator: [
        "inventory:read",
        "jobs:read",
        "artifacts:read",
        "jobs:submit",
        "jobs:cancel",
        "jobs:retry",
    ],
    Administrator: [
        "inventory:read",
        "jobs:read",
        "artifacts:read",
        "jobs:submit",
        "jobs:cancel",
        "jobs:retry",
        "connections:manage",
        "schedules:manage",
        "workers:manage",
        "grants:manage",
        "audit:read",
    ],
    MaintenanceService: [
        "inventory:read",
        "jobs:read",
        "artifacts:read",
        "jobs:submit",
        "jobs:retry",
    ],
};

function heldBy(role: string, scope: GrantScope): SessionPermission[] {
    return (ROLE_PERMISSIONS[role] ?? []).map((permission) => ({
        permission,
        scope,
    }));
}

/** A session holding `role` over `scope` (the whole organization by default). */
export function session(
    role = "Operator",
    scope: GrantScope = ORGANIZATION,
): SessionInfo {
    return {
        organization_id: "0190f3c4-0000-7000-8000-0000000000e1",
        principal_id: PRINCIPAL_ID,
        display_name: "Alice",
        // Far enough ahead that no journey sees the expiry notice.
        expires_at: "2099-01-01T00:00:00Z",
        grants: [{ role, scope }],
        permissions: heldBy(role, scope),
    };
}

export const catalogs = schema("CatalogPage", {
    items: [
        {
            id: CATALOG_ID,
            name: "lake",
            enabled: true,
            revision: 0,
            updated_at: "2026-10-01T00:00:00Z",
            created_at: "2026-10-01T00:00:00Z",
            settings: {
                maintenance_enabled: true,
                max_execution_seconds: 180,
                max_unresolved_publication_seconds: 3600,
            },
        },
    ],
    next_cursor: null,
});

export const catalogCapabilities = schema("CatalogCapabilities", {
    list_tables: "met",
    load_table: "met",
    commit_table: "met",
    vended_credentials: "unknown",
    credentials_endpoint: "missing",
    idempotency_key_lifetime_seconds: "9007199254740993",
});

export const managedCatalog = schema("Catalog", {
    id: CATALOG_ID,
    name: "lake",
    enabled: true,
    revision: 0,
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
    uri: "https://catalog.example",
    warehouse: "warehouse",
    credential_ref: null,
    settings: {
        maintenance_enabled: true,
        max_execution_seconds: 180,
        max_unresolved_publication_seconds: 3600,
    },
    capabilities: catalogCapabilities,
    capabilities_checked_at: "2026-10-24T00:00:00Z",
    storage_profile: {
        source: "default",
        backends: [
            {
                kind: "s3",
                region: "test",
                endpoint: "https://storage.example",
                path_style_access: true,
            },
        ],
    },
    unresolved_publications: { count: 1, oldest_seconds: 3601, overdue: true },
});

export const table = schema("Table", {
    id: TABLE_ID,
    table_uuid: "6f1c0d0e-1111-4222-8333-444455556666",
    created_at: "2026-10-01T00:00:00Z",
    aliases: [
        {
            catalog_id: CATALOG_ID,
            namespace: ["sales"],
            name: "orders",
            revision: 7,
        },
        {
            catalog_id: CATALOG_ID,
            namespace: ["legacy"],
            name: "orders_v1",
            revision: 8,
        },
    ],
    statistics: null,
    health: { state: null, sampled_at: null, confirmed_at: null },
});

/** A table's evaluated health; the null state is "not assessed". */
export function tableHealth(
    state: "healthy" | "warning" | "critical" | null,
): Schemas["TableHealthSummary"] {
    return schema("TableHealthSummary", {
        state,
        sampled_at: state === null ? null : "2026-10-26T00:00:00Z",
        confirmed_at: state === null ? null : "2026-10-26T00:05:00Z",
    });
}

/**
 * One table's findings and the action each recommends. The coordinator picks
 * one threshold set per assessment, so every finding shares its source.
 */
export function tableRecommendations(
    source: "defaults" | "policy" = "policy",
): Schemas["TableRecommendations"] {
    const findingSource: Schemas["HealthThresholdSource"] =
        source === "defaults"
            ? { kind: "defaults" }
            : { kind: "policy", policy_id: POLICY_ID, policy_name: "nightly" };
    return schema("TableRecommendations", {
        table_id: TABLE_ID,
        state: "critical",
        sampled_at: "2026-10-26T00:00:00Z",
        confirmed_at: "2026-10-26T00:05:00Z",
        findings: [
            {
                kind: "small_files",
                severity: "critical",
                measured: "4194304",
                threshold: "33554432",
                unit: "bytes",
                reason: "Live data files average 4.0 MiB, below the 32.0 MiB critical bound.",
                source: findingSource,
            },
            {
                kind: "snapshot_backlog",
                severity: "warning",
                measured: "1500",
                threshold: "1000",
                unit: "snapshots",
                reason: "The table retains 1,500 snapshots, at or above the 1,000 warning bound.",
                source: findingSource,
            },
        ],
        recommendations: [
            {
                kind: "small_files",
                action: "compaction",
                advisory: false,
                reason: null,
                assessment: "included",
                preview: { table_id: TABLE_ID },
                outcome: "planned",
                no_op_reason: null,
                estimate: {
                    tasks: "1",
                    files: "5",
                    bytes: "20971520",
                    files_removed: "5",
                    duration_millis: "120000",
                },
            },
            {
                kind: "snapshot_backlog",
                action: "snapshot_expiration",
                advisory: true,
                reason: "snapshot expiration is not implemented yet (BS-031)",
                assessment: "not_applicable",
                preview: null,
                outcome: null,
                no_op_reason: null,
                estimate: null,
            },
        ],
    });
}

/** One metadata-only sample; `rows` and `bytes` are decimal strings. */
export function tableSample(
    sampledAt: string,
    rows: string,
    bytes: string,
): Schemas["TableStatistics"] {
    return schema("TableStatistics", {
        sampled_at: sampledAt,
        catalog_connection_id: CATALOG_ID,
        snapshot_id: "3",
        last_operation: "append",
        record_count: rows,
        data_files: "3",
        data_bytes: bytes,
        delete_files: "1",
        delete_bytes: "4096",
        position_delete_files: "1",
        equality_delete_files: "0",
        position_delete_records: "10",
        equality_delete_records: "0",
        manifest_count: "2",
        manifest_bytes: "2048",
        partition_count: "2",
        small_files: "1",
        target_file_size_bytes: "1048576",
        snapshot_count: "4",
        oldest_snapshot_at: "2026-09-30T00:00:00Z",
        newest_snapshot_at: "2026-10-01T00:00:00Z",
        oldest_snapshot_age_seconds: "86400",
        newest_snapshot_age_seconds: "3600",
        size_histogram: [
            {
                lower_bound_bytes: "0",
                upper_bound_bytes: "1024",
                files: "1",
                bytes: "512",
            },
            {
                lower_bound_bytes: "1024",
                upper_bound_bytes: null,
                files: "2",
                bytes: "3584",
            },
        ],
        top_partitions: [
            {
                spec_id: 0,
                partition: "day=2026-10-01",
                files: "2",
                bytes: "2048",
                delete_files: "1",
            },
        ],
    });
}

/** A table definition with an old spec, a default spec, and one property. */
/** The landing overview: one unresolved publication and one conflict. */
export const overview = schema("Overview", {
    jobs: {
        window_hours: 24,
        ended: "3",
        by_state: {
            Succeeded: "2",
            NoOp: "0",
            Cancelled: "0",
            ReplanRequired: "1",
            Failed: "0",
        },
        conflict_rate_percent: "33.3",
    },
    attention: {
        unresolved_publications: "1",
        oldest_unresolved_age_seconds: "7200",
        terminal_jobs: "1",
        truncated: false,
        items: [
            {
                job_id: JOB_ID,
                table_id: TABLE_ID,
                state: "CommitOutcomeUnknown",
                reason: "unresolved_publication",
                age_seconds: "7200",
            },
            {
                job_id: RETRY_JOB_ID,
                table_id: TABLE_ID,
                state: "ReplanRequired",
                reason: "replan_required",
                age_seconds: "120",
            },
        ],
    },
    tables: {
        window_days: 30,
        total: "2",
        never_sampled: "1",
        latest_job_attention: "1",
    },
    schedules: {
        total: "1",
        enabled: "1",
        overdue: "1",
        oldest_overdue_age_seconds: "600",
        coalesced_last_day: "0",
        skipped_last_day: "0",
    },
    workers: {
        sessions: "2",
        live_sessions: "2",
        memory_bytes: "17179869184",
        cpu_millis: "8000",
        running_attempts: "1",
    },
});

export const tableDefinition = schema("TableDefinition", {
    table_id: TABLE_ID,
    format_version: "v2",
    schema: {
        schema_id: 0,
        fields: [
            {
                id: 1,
                name: "id",
                type: "long",
                required: true,
                doc: "row key",
            },
            {
                id: 2,
                name: "tag",
                type: "string",
                required: false,
                doc: null,
            },
        ],
    },
    partition_specs: [
        {
            spec_id: 0,
            default: false,
            fields: [
                {
                    source_id: 2,
                    field_id: 1000,
                    name: "tag",
                    transform: "identity",
                },
            ],
        },
        { spec_id: 1, default: true, fields: [] },
    ],
    sort_order: {
        order_id: 0,
        fields: [],
    },
    properties: {
        "write.format.default": "parquet",
        "write.target-file-size-bytes": "1048576",
    },
});

/** Two terminal jobs: one published, one discarded after a conflict. */
export const tableMaintenance = schema("TableMaintenancePage", {
    table_id: TABLE_ID,
    since: "2026-09-26T00:00:00Z",
    totals: {
        jobs: "2",
        succeeded: "1",
        no_op: "0",
        cancelled: "0",
        replanned: "1",
        failed: "0",
        input_files: "3",
        input_bytes: "127",
        output_files: "2",
        output_bytes: "2000",
        delete_files_applied: "2",
        discarded_files: "1",
        discarded_bytes: "4000",
        abandoned_files: "1",
        abandoned_bytes: "800",
    },
    items: [
        {
            job_id: JOB_ID,
            state: "ReplanRequired",
            created_at: "2026-10-02T00:00:00Z",
            updated_at: "2026-10-02T00:01:05Z",
            elapsed_millis: "65000",
            input_files: "1",
            input_bytes: "100",
            output_files: "1",
            output_bytes: "4000",
            delete_files_applied: "0",
        },
        {
            job_id: RETRY_JOB_ID,
            state: "Succeeded",
            created_at: "2026-10-01T00:00:00Z",
            updated_at: "2026-10-01T00:02:30Z",
            elapsed_millis: "150000",
            input_files: "2",
            input_bytes: "27",
            output_files: "2",
            output_bytes: "2000",
            delete_files_applied: "1",
        },
    ],
    next_cursor: null,
});

export const tableStatistics = schema("TableStatisticsPage", {
    items: [
        tableSample("2026-10-02T00:00:00Z", "200", "4096"),
        tableSample("2026-10-01T00:00:00Z", "120", "2048"),
    ],
    next_cursor: null,
    latest: tableSample("2026-10-02T00:00:00Z", "200", "4096"),
});

/** One recorded commit; `job` marks a Bergsmith publication. */
export function tableSnapshot(
    snapshotId: string,
    committedAt: string,
    operation: string,
    addedFiles: string,
    addedRecords: string,
    job: string | null,
): Schemas["TableSnapshot"] {
    return schema("TableSnapshot", {
        snapshot_id: snapshotId,
        parent_snapshot_id: null,
        sequence_number: snapshotId,
        committed_at: committedAt,
        operation,
        delta: {
            added_data_files: addedFiles,
            deleted_data_files: "0",
            added_records: addedRecords,
            deleted_records: "0",
            added_bytes: "2048",
            removed_bytes: "0",
            added_delete_files: "0",
            removed_delete_files: "0",
            added_position_deletes: "0",
            removed_position_deletes: "0",
            added_equality_deletes: "0",
            removed_equality_deletes: "0",
        },
        job_id: job,
        properties: {
            "engine-name": "spark",
            "engine-version": "4.0.0",
        },
    });
}

/** A day of hourly buckets; the sixth hour carries the two fixture commits. */
export const tableSnapshots = schema("TableSnapshotPage", {
    items: [
        tableSnapshot("9", "2026-10-01T05:30:00Z", "append", "2", "40", JOB_ID),
        tableSnapshot("8", "2026-10-01T05:05:00Z", "replace", "3", "60", null),
    ],
    next_cursor: null,
    cadence: {
        hours: 24,
        buckets: Array.from({ length: 24 }, (_, index) =>
            schema("SnapshotHour", {
                start: `2026-10-01T${String(index).padStart(2, "0")}:00:00Z`,
                commits: index === 5 ? "2" : "0",
            }),
        ),
    },
});

/** A table with no recorded commits: a day of empty buckets. */
export const tableSnapshotsQuiet = schema("TableSnapshotPage", {
    items: [],
    next_cursor: null,
    cadence: {
        hours: 24,
        buckets: Array.from({ length: 24 }, (_, index) =>
            schema("SnapshotHour", {
                start: `2026-10-01T${String(index).padStart(2, "0")}:00:00Z`,
                commits: "0",
            }),
        ),
    },
});

export const catalogTotals = schema("StatisticsTotals", {
    tables: "2",
    record_count: "1200",
    data_files: "12",
    data_bytes: "1048576",
    delete_files: "1",
    delete_bytes: "4096",
    small_files: "3",
    snapshot_count: "8",
    last_sampled_at: "2026-10-02T00:00:00Z",
});

export function job(state: JobState, overrides: Partial<Job> = {}): Job {
    return schema("Job", {
        id: JOB_ID,
        table_id: TABLE_ID,
        table_aliases: table.aliases,
        state,
        revision: 1,
        state_reason: null,
        retry_of_job_id: null,
        created_by: PRINCIPAL_ID,
        created_by_name: "Alice",
        created_at: "2026-10-21T10:00:00Z",
        updated_at: "2026-10-21T10:05:00Z",
        cancellation_requested: false,
        publication_started: false,
        trigger: null,
        ...overrides,
    });
}

export function event(
    sequence: number,
    to: JobState,
    from: JobState | null,
    kind: JobEvent["kind"] = from === null ? "created" : "transitioned",
): JobEvent {
    return schema("JobEvent", {
        sequence,
        kind,
        from_state: from,
        to_state: to,
        state_revision: sequence - 1,
        reason: null,
        actor_id: null,
        occurred_at: "2026-10-21T10:00:00Z",
    });
}

export const plannedPreview = schema("Preview", {
    table_id: TABLE_ID,
    outcome: "planned",
    no_op_reason: null,
    // Above 2^53: a JSON number would round it.
    baseline: { snapshot_id: "9007199254740993", sequence_number: 42 },
    rejections: [],
    candidates: { files: 12, bytes: 12 * 1024 * 1024 },
    skipped: [
        { reason: "at_target_size", files: 3, bytes: 3 * 512 * 1024 * 1024 },
    ],
    bins: [
        {
            partition: {
                spec_id: 0,
                values: [{ field_id: 1000, value: "2026-10-20" }],
            },
            ordinal: 0,
            input: { files: 12, bytes: 12 * 1024 * 1024 },
            has_deletes: true,
            files_removed: 11,
            disposition: "admitted",
        },
    ],
    estimate: {
        tasks: 1,
        files: 12,
        bytes: 12 * 1024 * 1024,
        files_removed: 11,
        duration_millis: 95_000,
    },
    bins_total: 4,
    bins_truncated: true,
});

export const rejectedPreview = schema("Preview", {
    ...plannedPreview,
    outcome: "rejected",
    baseline: null,
    rejections: [
        {
            reason: "sorted_table",
            detail: "the table declares a sort order",
            path: null,
            version: null,
            sort_order_id: 1,
            format: null,
            field_id: null,
            problem: null,
        },
    ],
    bins: [],
    bins_total: 0,
    bins_truncated: false,
    skipped: [],
});

/** A policy at the planner defaults, with a revision to edit against. */
export function policy(
    overrides: Partial<Schemas["MaintenancePolicy"]> = {},
): Schemas["MaintenancePolicy"] {
    return schema("MaintenancePolicy", {
        id: POLICY_ID,
        name: "nightly-compact",
        operation: "Compact",
        target_file_size_bytes: 268_435_456,
        min_input_files: 5,
        max_input_bytes: 107_374_182_400,
        health_thresholds: {
            small_files: { warning: 134_217_728, critical: 33_554_432 },
            delete_pressure: { warning: 100, critical: 500 },
            snapshot_backlog: { warning: 1_000, critical: 5_000 },
            manifest_fragmentation: { warning: 1_000, critical: 100 },
            partition_skew: { warning: 500, critical: 800 },
            stale_statistics: { warning: 3_600, critical: 21_600 },
        },
        health_trigger: null,
        enabled: true,
        revision: 3,
        created_at: "2026-10-01T00:00:00Z",
        updated_at: "2026-10-20T00:00:00Z",
        ...overrides,
    });
}

export const policies = schema("MaintenancePolicyPage", {
    items: [policy()],
    next_cursor: null,
});

/** A service identity, as the schedule picker lists it. */
export function serviceIdentity(
    overrides: Partial<Schemas["Principal"]> = {},
): Schemas["Principal"] {
    return schema("Principal", {
        id: SERVICE_ID,
        kind: "Service",
        issuer: "bergsmith:test",
        subject: "scheduler",
        display_name: "scheduler",
        active: true,
        created_at: "2026-10-01T00:00:00Z",
        deactivated_at: null,
        ...overrides,
    });
}

/** An hourly schedule with a daily window and two computed instants. */
export function schedule(
    overrides: Partial<Schemas["MaintenanceSchedule"]> = {},
): Schemas["MaintenanceSchedule"] {
    return schema("MaintenanceSchedule", {
        id: SCHEDULE_ID,
        policy_id: POLICY_ID,
        table_id: TABLE_ID,
        cron_expression: "0 2 * * *",
        time_zone: "Europe/Berlin",
        window: { start_minute: 120, end_minute: 300 },
        service_principal_id: SERVICE_ID,
        service_identity_name: "scheduler",
        enabled: true,
        next_occurrences: [
            "2026-10-26T01:00:00.000Z",
            "2026-10-27T01:00:00.000Z",
        ],
        revision: 2,
        created_at: "2026-10-01T00:00:00Z",
        updated_at: "2026-10-20T00:00:00Z",
        ...overrides,
    });
}

export const schedules = schema("MaintenanceSchedulePage", {
    items: [schedule()],
    next_cursor: null,
});

/** One recorded evaluation; the default admitted run points at the job. */
export function occurrence(
    overrides: Partial<Schemas["ScheduleOccurrence"]> = {},
): Schemas["ScheduleOccurrence"] {
    return schema("ScheduleOccurrence", {
        id: OCCURRENCE_ID,
        scheduled_for: "2026-10-26T01:00:00Z",
        coalesced_until: null,
        state: "Admitted",
        job_id: JOB_ID,
        reason: null,
        created_at: "2026-10-26T01:00:01Z",
        ...overrides,
    });
}

export const scheduleOccurrences = schema("ScheduleOccurrencePage", {
    items: [
        occurrence(),
        occurrence({
            id: "0190f3c4-0000-7000-8000-0000000000d4",
            scheduled_for: "2026-10-25T01:00:00Z",
            state: "Skipped",
            job_id: null,
            reason: "OutsideWindow",
        }),
    ],
    next_cursor: null,
});
