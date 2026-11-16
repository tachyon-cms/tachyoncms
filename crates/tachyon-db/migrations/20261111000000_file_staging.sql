-- Where a client puts a file it was issued instructions for: a key of its own, never the
-- file's, which only the server writes once it has checked what was put.
ALTER TABLE files ADD COLUMN staging_key text;
