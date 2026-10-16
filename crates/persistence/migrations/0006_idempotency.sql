-- BS-003.T7: idempotency keys, unique within an organization and bound to
-- the hash of the request that first used them.

CREATE TABLE idempotency_keys (
    organization_id uuid NOT NULL REFERENCES organizations (id),
    key text NOT NULL CHECK (length(key) BETWEEN 1 AND 255),
    operation text NOT NULL CHECK (length(operation) BETWEEN 1 AND 100),
    request_hash text NOT NULL CHECK (request_hash ~ '^[0-9a-f]{64}$'),
    resource_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (organization_id, key)
);

CREATE TRIGGER idempotency_keys_are_immutable BEFORE UPDATE OR DELETE ON idempotency_keys
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
