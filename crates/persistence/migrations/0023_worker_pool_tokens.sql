-- BS-023.T4: worker pool tokens are issued, rotated, and revoked in PostgreSQL.
--
-- Only the domain-separated verifier is stored. The bearer token is returned
-- once when it is issued and never persisted or listed again. A row stays after
-- revocation so the audit trail can name the token that authenticated a worker.
CREATE TABLE worker_pool_tokens (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL,
    principal_id uuid NOT NULL,
    -- Matches ServiceToken's pool syntax: lowercase letters, digits, and
    -- hyphens, starting with a letter or digit.
    pool text NOT NULL CHECK (pool ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    -- SHA-256("bergsmith.service-token.v1" || 0x00 || token). Unique because a
    -- repeated bearer token must never map to two identities.
    verifier bytea NOT NULL UNIQUE CHECK (octet_length(verifier) = 32),
    label text CHECK (label IS NULL OR length(label) BETWEEN 1 AND 200),
    created_by uuid,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    -- Set on the replacement when rotation revokes its predecessor.
    rotated_from uuid,
    revoked_by uuid,
    revoked_at timestamptz,
    CHECK (revoked_by IS NULL OR revoked_at IS NOT NULL),
    UNIQUE (organization_id, id),
    FOREIGN KEY (organization_id, principal_id)
        REFERENCES principals (organization_id, id),
    FOREIGN KEY (organization_id, created_by)
        REFERENCES principals (organization_id, id),
    FOREIGN KEY (organization_id, revoked_by)
        REFERENCES principals (organization_id, id),
    FOREIGN KEY (organization_id, rotated_from)
        REFERENCES worker_pool_tokens (organization_id, id)
);

CREATE INDEX worker_pool_tokens_by_principal
    ON worker_pool_tokens (organization_id, principal_id, id);
CREATE INDEX worker_pool_tokens_active_by_pool
    ON worker_pool_tokens (pool) WHERE revoked_at IS NULL;
