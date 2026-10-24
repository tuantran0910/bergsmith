-- Job detail aggregates accepted output inventory by attempt, without scanning
-- every job's retained objects. Include size so the aggregate can use this index.
CREATE INDEX attempt_output_objects_by_attempt
    ON attempt_output_objects (attempt_id) INCLUDE (size_bytes);
