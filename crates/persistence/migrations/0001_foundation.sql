-- BS-003.T1: shared helpers for the execution ledger.
--
-- Migrations are applied only by `bergsmith-coordinator migrate`, never by
-- replicas at startup. Never edit an applied migration; add a new one.

-- Rejects UPDATE and DELETE on append-only or immutable tables (plans,
-- receipts, job events). Attach it with a BEFORE UPDATE OR DELETE trigger.
CREATE FUNCTION bergsmith_reject_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'rows in % are immutable', TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation';
END;
$$;
