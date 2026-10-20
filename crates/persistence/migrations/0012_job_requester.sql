-- BS-017 follow-up: attribute legacy jobs and make a requester mandatory.
--
-- Jobs created before scoped authorization had no `created_by`. Rather than
-- keep an unattributed authorization bypass, each affected organization gets
-- one explicit `bergsmith:system` service principal. Legacy rows are
-- attributed to it, and it receives an organization-wide
-- `MaintenanceService` grant so in-flight work finishes.
--
-- `created_by` then becomes NOT NULL, so a writer that forgets the requester
-- fails the insert instead of silently skipping authorization. Revoking the
-- system grant is the documented way to halt legacy work.

INSERT INTO principals (id, organization_id, kind, issuer, subject, display_name)
SELECT gen_random_uuid(), j.organization_id, 'Service', 'bergsmith:system', 'system',
       'legacy job attribution'
FROM jobs j
WHERE j.created_by IS NULL
  AND NOT EXISTS (
      SELECT 1 FROM principals p
      WHERE p.organization_id = j.organization_id
        AND p.issuer = 'bergsmith:system'
        AND p.subject = 'system')
GROUP BY j.organization_id;

UPDATE jobs j
SET created_by = p.id
FROM principals p
WHERE j.created_by IS NULL
  AND p.organization_id = j.organization_id
  AND p.issuer = 'bergsmith:system'
  AND p.subject = 'system';

INSERT INTO principal_role_grants (id, organization_id, principal_id, role, granted_by)
SELECT gen_random_uuid(), p.organization_id, p.id, 'MaintenanceService', NULL
FROM principals p
WHERE p.issuer = 'bergsmith:system'
  AND p.subject = 'system'
  AND NOT EXISTS (
      SELECT 1 FROM principal_role_grants g
      WHERE g.organization_id = p.organization_id
        AND g.principal_id = p.id
        AND g.revoked_at IS NULL);

INSERT INTO audit_events
    (organization_id, actor_principal_id, actor_kind, action, resource_type,
     resource_id, outcome, detail)
SELECT p.organization_id, p.id, 'System', 'identity.provisioned', 'principal',
       p.id::text, 'Allowed', '{"backfill": "legacy-job-attribution"}'::jsonb
FROM principals p
WHERE p.issuer = 'bergsmith:system'
  AND p.subject = 'system';

-- The backfill above rewrites every legacy row and this takes an ACCESS
-- EXCLUSIVE lock on `jobs`. The migrate command runs before coordinators start,
-- so this is a bounded one-time upgrade cost; on a large table, run the
-- migration job in a maintenance window.
ALTER TABLE jobs ALTER COLUMN created_by SET NOT NULL;
