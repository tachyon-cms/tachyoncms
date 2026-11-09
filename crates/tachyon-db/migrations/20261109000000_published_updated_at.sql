-- When the published copy last changed. `updated_at` moves on every write, a draft-only
-- save included, so a public read showing it would tell readers a draft changed.
ALTER TABLE documents ADD COLUMN published_updated_at timestamptz;
UPDATE documents SET published_updated_at = updated_at;
ALTER TABLE documents ALTER COLUMN published_updated_at SET NOT NULL,
                      ALTER COLUMN published_updated_at SET DEFAULT now();
