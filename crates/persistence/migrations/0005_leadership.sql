-- BS-003.T6: coordinator leadership lease and durable epoch.
--
-- One row. A coordinator holds leadership while its lease is live by
-- database time. Each new holder gets a strictly larger epoch, so work
-- authorized under an older epoch can be rejected.

CREATE TABLE coordinator_leadership (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    holder text NOT NULL CHECK (length(holder) BETWEEN 1 AND 200),
    epoch bigint NOT NULL CHECK (epoch >= 1),
    acquired_at timestamptz NOT NULL,
    lease_expires_at timestamptz NOT NULL,
    CHECK (lease_expires_at >= acquired_at)
);
