-- BS-029.T1: notification destinations (ADR 0060).
--
-- An organization may register webhook or Slack-compatible endpoints that
-- receive events. The row holds a reference to the signing secret, never the
-- secret itself: the coordinator reads notifications.secrets_dir/<reference>
-- when it sends. A webhook is always signed; a Slack body carries no
-- Bergsmith signature, so exactly one of the two shapes names a reference.
--
-- The URL is validated against notifications.allowed_hosts by the coordinator
-- before insert and before every send, so this table only bounds its length.
CREATE TABLE notification_destinations (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    name text NOT NULL CHECK (name ~ '^[A-Za-z0-9][A-Za-z0-9 ._-]{0,199}$'),
    kind text NOT NULL CHECK (kind IN ('webhook', 'slack')),
    url text NOT NULL CHECK (length(url) BETWEEN 1 AND 2048),
    signing_secret_ref text
        CHECK (signing_secret_ref ~ '^[A-Za-z0-9._-]{1,128}$'),
    enabled boolean NOT NULL DEFAULT true,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_by uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, name),
    FOREIGN KEY (organization_id, created_by)
        REFERENCES principals (organization_id, id),
    CONSTRAINT notification_destinations_signing
        CHECK ((kind = 'webhook') = (signing_secret_ref IS NOT NULL)),
    CONSTRAINT notification_destinations_secret_not_directory
        CHECK (signing_secret_ref IS NULL OR signing_secret_ref NOT IN ('.', '..'))
);

-- Listing reads `organization_id = $1 AND id > $2 ORDER BY id`, which the
-- `UNIQUE (organization_id, id)` constraint already indexes.
