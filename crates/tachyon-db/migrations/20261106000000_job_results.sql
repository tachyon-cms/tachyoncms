-- What a finished job answers whoever queued it, such as a schema change's outcome or the
-- problem that stopped it. Null until it finishes, and for jobs nobody waits on.
ALTER TABLE jobs ADD COLUMN result jsonb;
