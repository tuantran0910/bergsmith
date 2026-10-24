-- Development bootstrap for the Compose application stack (BS-020.T1).
--
-- Every identifier and credential here is fake and development-only. The
-- organization and service principal identifiers match coordinator.json;
-- the administrator subject matches the Keycloak fixture user. Run this once
-- after migrations, before the coordinator starts.
INSERT INTO organizations (id, slug, display_name)
VALUES ('01900000-0000-7000-8000-000000000001', 'development', 'Development')
ON CONFLICT DO NOTHING;

INSERT INTO principals (id, organization_id, kind, issuer, subject, display_name)
VALUES (
    '01900000-0000-7000-8000-000000000002',
    '01900000-0000-7000-8000-000000000001',
    'User',
    'http://idp.localtest.me:58081/realms/bergsmith',
    '01900000-0000-7000-8000-000000000005',
    'Alice Operator')
ON CONFLICT DO NOTHING;

INSERT INTO principal_role_grants (id, organization_id, principal_id, role)
VALUES (
    '01900000-0000-7000-8000-000000000010',
    '01900000-0000-7000-8000-000000000001',
    '01900000-0000-7000-8000-000000000002',
    'Administrator')
ON CONFLICT DO NOTHING;

INSERT INTO principals (id, organization_id, kind, issuer, subject, display_name)
VALUES (
    '01900000-0000-7000-8000-000000000003',
    '01900000-0000-7000-8000-000000000001',
    'Service',
    'bergsmith:service',
    '01900000-0000-7000-8000-000000000003',
    'Native workers')
ON CONFLICT DO NOTHING;

-- The verifier matches deploy/docker/worker-token. This is a development-only
-- credential; production tokens are issued through the API and stored the same
-- way (verifier only).
INSERT INTO worker_pool_tokens
    (id, organization_id, principal_id, pool, verifier, label)
VALUES (
    '01900000-0000-7000-8000-000000000011',
    '01900000-0000-7000-8000-000000000001',
    '01900000-0000-7000-8000-000000000003',
    'native',
    decode('15f607e06466620da32baf750d8be9a88ccdf774ef2aa532a7f68c8d9e6c8728', 'hex'),
    'development workers')
ON CONFLICT (verifier) DO NOTHING;

INSERT INTO catalog_connections (id, organization_id, name, uri, warehouse)
VALUES (
    '01900000-0000-7000-8000-000000000004',
    '01900000-0000-7000-8000-000000000001',
    'development',
    'http://lakekeeper:8181/catalog',
    'development')
ON CONFLICT (id) DO UPDATE SET warehouse = EXCLUDED.warehouse;
