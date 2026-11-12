-- Each upload instruction queues a staging sweep; the per-client cap counts the recent ones.
CREATE INDEX jobs_staging_sweeps ON jobs (collection_id, created_by, created_at)
    WHERE kind = 'file_cleanup' AND payload ? 'staging';
