import { schema } from "./fakes";
import { PRINCIPAL_ID } from "./fixtures";

export const USER_ID = "0190f3c4-0000-7000-8000-000000000101";
export const SERVICE_ID = "0190f3c4-0000-7000-8000-000000000102";
export const GRANT_ID = "0190f3c4-0000-7000-8000-000000000103";
export const TOKEN_ID = "0190f3c4-0000-7000-8000-000000000104";
export const userPrincipal = schema("Principal", {
    id: USER_ID,
    kind: "User",
    issuer: "https://id.example",
    subject: "bob-subject",
    display_name: "Bob",
    active: true,
    created_at: "2026-10-24T00:00:00Z",
});
export const servicePrincipal = schema("Principal", {
    ...userPrincipal,
    id: SERVICE_ID,
    kind: "Service",
    issuer: "bergsmith:service",
    subject: SERVICE_ID,
    display_name: "Compaction workers",
});
export const roleGrant = schema("RoleGrant", {
    id: GRANT_ID,
    principal_id: USER_ID,
    role: "Viewer",
    scope: {},
    granted_by: PRINCIPAL_ID,
    granted_at: "2026-10-24T00:00:00Z",
});
export const poolToken = schema("WorkerPoolToken", {
    id: TOKEN_ID,
    principal_id: SERVICE_ID,
    pool: "default",
    label: "Worker fleet",
    active: true,
    created_at: "2026-10-24T00:00:00Z",
});
// Fake bearer used only by tests. Never copy a deployment credential here.
export const issuedPoolToken = schema("IssuedWorkerPoolToken", {
    ...poolToken,
    token: "bsw1_default_fake-test-bearer",
});
