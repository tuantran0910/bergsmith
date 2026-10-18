-- Every generation of a task must execute within its original resource envelope.
CREATE TABLE task_resource_envelopes (
    task_id uuid PRIMARY KEY REFERENCES tasks (id),
    content bytea NOT NULL CHECK (octet_length(content) > 0)
);
CREATE TRIGGER task_resource_envelopes_are_immutable
    BEFORE UPDATE OR DELETE ON task_resource_envelopes
    FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();
