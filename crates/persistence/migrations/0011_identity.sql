-- BS-017: OIDC login state, server-side sessions, scoped role grants, and the
-- append-only audit log.
--
-- Credentials, tokens, and session secrets are never stored in clear form:
-- sessions and login states are keyed by the SHA-256 of a random token. The
-- PKCE verifier is the one login-time exception: the authorization server
-- needs it during the code exchange, so it lives only until that exchange
-- consumes the row.

CREATE TABLE principal_role_grants (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL,
    principal_id uuid NOT NULL,
    role text NOT NULL CHECK (role IN (
        'Viewer', 'Operator', 'Administrator', 'MaintenanceService')),
    -- NULL/NULL covers the whole organization, one NULL covers exactly one
    -- connection or table. A grant never names both.
    catalog_connection_id uuid,
    table_identity_id uuid,
    granted_by uuid,
    granted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    revoked_by uuid,
    revoked_at timestamptz,
    CHECK (catalog_connection_id IS NULL OR table_identity_id IS NULL),
    CHECK (revoked_by IS NULL OR revoked_at IS NOT NULL),
    FOREIGN KEY (organization_id, principal_id)
        REFERENCES principals (organization_id, id),
    FOREIGN KEY (organization_id, catalog_connection_id)
        REFERENCES catalog_connections (organization_id, id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id),
    FOREIGN KEY (organization_id, granted_by)
        REFERENCES principals (organization_id, id),
    FOREIGN KEY (organization_id, revoked_by)
        REFERENCES principals (organization_id, id)
);

-- At most one active grant per principal, role, and scope. NULLs would defeat
-- a plain unique constraint, so the scope is coalesced to the nil UUID.
CREATE UNIQUE INDEX principal_role_grants_one_active
    ON principal_role_grants (
        organization_id, principal_id, role,
        COALESCE(catalog_connection_id, '00000000-0000-0000-0000-000000000000'::uuid),
        COALESCE(table_identity_id, '00000000-0000-0000-0000-000000000000'::uuid))
    WHERE revoked_at IS NULL;
CREATE INDEX principal_role_grants_by_principal
    ON principal_role_grants (organization_id, principal_id)
    WHERE revoked_at IS NULL;

CREATE TABLE auth_sessions (
    token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    organization_id uuid NOT NULL,
    principal_id uuid NOT NULL,
    -- Identity binding at issue time; a later subject change cannot inherit
    -- this session.
    issuer text NOT NULL CHECK (length(issuer) > 0),
    subject text NOT NULL CHECK (length(subject) > 0),
    csrf_token_hash bytea NOT NULL CHECK (octet_length(csrf_token_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    last_seen_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    revoked_reason text,
    CHECK (expires_at > created_at),
    CHECK (last_seen_at >= created_at),
    CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)),
    FOREIGN KEY (organization_id, principal_id)
        REFERENCES principals (organization_id, id)
);
CREATE INDEX auth_sessions_by_principal
    ON auth_sessions (organization_id, principal_id) WHERE revoked_at IS NULL;
CREATE INDEX auth_sessions_expiry ON auth_sessions (expires_at);

-- One in-flight authorization-code exchange. `state_hash` is the lookup key;
-- the raw state is only ever sent to the browser. `binding_hash` binds the
-- exchange to the browser that started it, so a state captured by an attacker
-- cannot be redeemed in a victim's browser. The row is deleted when consumed.
CREATE TABLE auth_login_states (
    state_hash bytea PRIMARY KEY CHECK (octet_length(state_hash) = 32),
    binding_hash bytea NOT NULL CHECK (octet_length(binding_hash) = 32),
    organization_id uuid NOT NULL REFERENCES organizations (id),
    -- Relative application path only: never an absolute or scheme-relative URL,
    -- and never a value a browser can normalize to one (backslash, whitespace).
    return_to text NOT NULL
        CHECK (return_to LIKE '/%' AND return_to NOT LIKE '//%'
               AND length(return_to) <= 2048
               AND return_to !~ '[[:space:]\\]'),
    nonce text NOT NULL CHECK (length(nonce) BETWEEN 16 AND 128),
    pkce_verifier text NOT NULL CHECK (pkce_verifier ~ '^[A-Za-z0-9_-]{43}$'),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    expires_at timestamptz NOT NULL,
    CHECK (expires_at > created_at)
);
CREATE INDEX auth_login_states_expiry ON auth_login_states (expires_at);
CREATE INDEX auth_login_states_by_organization
    ON auth_login_states (organization_id, expires_at);

CREATE TABLE audit_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    organization_id uuid REFERENCES organizations (id),
    actor_principal_id uuid,
    actor_kind text NOT NULL CHECK (actor_kind IN (
        'User', 'Service', 'Anonymous', 'System')),
    action text NOT NULL CHECK (length(action) BETWEEN 1 AND 100),
    resource_type text NOT NULL CHECK (length(resource_type) BETWEEN 1 AND 100),
    resource_id text,
    outcome text NOT NULL CHECK (outcome IN ('Allowed', 'Denied', 'Failed')),
    -- Structured context only. Typed callers construct this object; the
    -- check below rejects the obvious credential keys as a second line.
    detail jsonb NOT NULL DEFAULT '{}'::jsonb,
    occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (jsonb_typeof(detail) = 'object'),
    CHECK (NOT (detail ?| ARRAY[
        'token', 'access_token', 'refresh_token', 'id_token', 'secret',
        'password', 'client_secret', 'authorization', 'cookie', 'csrf'])),
    CHECK (actor_principal_id IS NULL OR organization_id IS NOT NULL),
    FOREIGN KEY (organization_id, actor_principal_id)
        REFERENCES principals (organization_id, id)
);
CREATE INDEX audit_events_by_organization
    ON audit_events (organization_id, occurred_at, id);
-- The listing order is by identifier, so give that its own index; the
-- time-ordered index above serves time-range queries.
CREATE INDEX audit_events_by_organization_id
    ON audit_events (organization_id, id);
CREATE TRIGGER audit_events_are_immutable BEFORE UPDATE OR DELETE ON audit_events
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
