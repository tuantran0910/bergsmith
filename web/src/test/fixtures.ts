// Contract-typed fixtures. A change to the OpenAPI document that these no
// longer satisfy fails the typecheck, not just a test.

import type { Job, JobEvent, JobState } from "../api/client";
import type { SessionInfo } from "../api/auth";
import { schema } from "./fakes";

export const TABLE_ID = "0190f3c4-0000-7000-8000-000000000001";
export const CATALOG_ID = "0190f3c4-0000-7000-8000-0000000000c1";
export const JOB_ID = "0190f3c4-0000-7000-8000-00000000000a";
export const RETRY_JOB_ID = "0190f3c4-0000-7000-8000-00000000000b";
export const PRINCIPAL_ID = "0190f3c4-0000-7000-8000-0000000000aa";

export function session(role = "Operator"): SessionInfo {
    return {
        organization_id: "0190f3c4-0000-7000-8000-0000000000e1",
        principal_id: PRINCIPAL_ID,
        display_name: "Alice",
        expires_at: "2026-10-22T00:00:00Z",
        grants: [{ role, scope: { catalog_connection: null, table: null } }],
    };
}

export const catalogs = schema("CatalogPage", {
    items: [
        { id: CATALOG_ID, name: "lake", created_at: "2026-10-01T00:00:00Z" },
    ],
    next_cursor: null,
});

export const table = schema("Table", {
    id: TABLE_ID,
    table_uuid: "6f1c0d0e-1111-4222-8333-444455556666",
    created_at: "2026-10-01T00:00:00Z",
    aliases: [
        { catalog_id: CATALOG_ID, namespace: ["sales"], name: "orders" },
        { catalog_id: CATALOG_ID, namespace: ["legacy"], name: "orders_v1" },
    ],
});

export function job(state: JobState, overrides: Partial<Job> = {}): Job {
    return schema("Job", {
        id: JOB_ID,
        table_id: TABLE_ID,
        state,
        revision: 1,
        state_reason: null,
        retry_of_job_id: null,
        created_by: PRINCIPAL_ID,
        created_at: "2026-10-21T10:00:00Z",
        updated_at: "2026-10-21T10:05:00Z",
        cancellation_requested: false,
        publication_started: false,
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
