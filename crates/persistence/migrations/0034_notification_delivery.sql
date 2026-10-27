-- BS-029.T3: durable notification delivery (ADR 0060).
--
-- A rule match appends one delivery in the same transaction as the event, so a
-- crash between the event and the send loses nothing, and a rolled-back event
-- announces nothing. The leader claims due rows with SKIP LOCKED, sends outside
-- any transaction, then records the attempt and the next attempt instant.
CREATE TABLE notification_deliveries (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations (id),
    rule_id uuid NOT NULL,
    destination_id uuid NOT NULL,
    event_kind text NOT NULL,
    -- The event that produced the row, for duplicate suppression.
    source_key text NOT NULL CHECK (length(source_key) BETWEEN 1 AND 300),
    occurred_at timestamptz NOT NULL,
    summary text NOT NULL CHECK (length(summary) BETWEEN 1 AND 500),
    resource_kind text NOT NULL CHECK (length(resource_kind) BETWEEN 1 AND 100),
    resource_id text NOT NULL CHECK (length(resource_id) BETWEEN 1 AND 200),
    resource_label text,
    reason text,
    state text NOT NULL DEFAULT 'Pending'
        CHECK (state IN ('Pending', 'Delivered', 'Exhausted')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    -- A claim that never reports back (crash mid-send) retries after it
    -- expires; the token makes a stale sender's completion a no-op.
    claimed_until timestamptz,
    claim_token uuid NOT NULL DEFAULT gen_random_uuid(),
    last_error text,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (organization_id, rule_id, source_key),
    FOREIGN KEY (organization_id, rule_id)
        REFERENCES notification_rules (organization_id, id),
    FOREIGN KEY (organization_id, destination_id)
        REFERENCES notification_destinations (organization_id, id)
);
CREATE INDEX notification_deliveries_due
    ON notification_deliveries (next_attempt_at) WHERE state = 'Pending';
CREATE INDEX notification_deliveries_prune
    ON notification_deliveries (updated_at)
    WHERE state IN ('Delivered', 'Exhausted');

CREATE TABLE notification_attempts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    delivery_id uuid NOT NULL
        REFERENCES notification_deliveries (id) ON DELETE CASCADE,
    attempt integer NOT NULL CHECK (attempt >= 1),
    finished_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    outcome text NOT NULL CHECK (outcome IN ('Delivered', 'Rejected', 'Failed')),
    status integer,
    reason text,
    UNIQUE (delivery_id, attempt)
);

-- A repeat threshold counts occurrences per rule and resource. The counter
-- resets when a notification is enqueued and when the last occurrence is older
-- than the window, so a stale burst does not fire a later, unrelated event.
CREATE TABLE notification_rule_counters (
    organization_id uuid NOT NULL,
    rule_id uuid NOT NULL,
    counter_key text NOT NULL,
    occurrences integer NOT NULL CHECK (occurrences >= 0),
    last_occurred_at timestamptz NOT NULL,
    PRIMARY KEY (organization_id, rule_id, counter_key),
    FOREIGN KEY (organization_id, rule_id)
        REFERENCES notification_rules (organization_id, id)
);

-- Every distinct source a thresholded rule has counted, so a replay below the
-- threshold cannot advance the counter. Rows are pruned with the deliveries.
CREATE TABLE notification_rule_events (
    organization_id uuid NOT NULL,
    rule_id uuid NOT NULL,
    source_key text NOT NULL CHECK (length(source_key) BETWEEN 1 AND 300),
    occurred_at timestamptz NOT NULL,
    PRIMARY KEY (organization_id, rule_id, source_key),
    FOREIGN KEY (organization_id, rule_id)
        REFERENCES notification_rules (organization_id, id)
);
CREATE INDEX notification_rule_events_occurred
    ON notification_rule_events (occurred_at);

-- The last health state the collector observed per table. A Critical alert
-- fires only when the state enters Critical, and recovery lets a later entry
-- alert again.
CREATE TABLE table_health_states (
    organization_id uuid NOT NULL,
    table_identity_id uuid NOT NULL,
    state text CHECK (state IN ('healthy', 'warning', 'critical')),
    observed_at timestamptz NOT NULL,
    PRIMARY KEY (organization_id, table_identity_id),
    FOREIGN KEY (organization_id, table_identity_id)
        REFERENCES table_identities (organization_id, id)
);
