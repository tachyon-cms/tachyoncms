-- Stored files: one row per original, with its sizes. A file lives while a published copy,
-- a draft or a kept version points at it; when the last goes it turns `gone` and a
-- `file_cleanup` job deletes it from storage.
CREATE TABLE files (
    id            uuid PRIMARY KEY,
    tenant_id     uuid NOT NULL REFERENCES tenants (id),
    collection_id uuid NOT NULL,
    storage       text NOT NULL,
    key           text NOT NULL,
    filename      text NOT NULL,
    -- The `filenameCompoundIndex` values as JSON, or '': names are numbered within one.
    scope         text NOT NULL DEFAULT '',
    mime_type     text NOT NULL,
    filesize      bigint NOT NULL,
    width         int,
    height        int,
    focal_x       real NOT NULL DEFAULT 50,
    focal_y       real NOT NULL DEFAULT 50,
    blurhash      text,
    sizes         jsonb NOT NULL DEFAULT '{}',
    -- Every name it's served under: the original's and each size's.
    names         text[] NOT NULL,
    -- `pending` until a document takes it, then `live`, then `gone` once nothing does.
    state         text NOT NULL CHECK (state IN ('pending', 'live', 'gone')),
    -- Who may claim a pending file uploaded straight to the bucket: a user, and their key.
    issued_to     uuid,
    issued_key    uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    released_at   timestamptz,
    FOREIGN KEY (tenant_id, collection_id) REFERENCES collections (tenant_id, id)
);

-- Not unique: a clash is numbered (`a-1.png`) by `files::reserve`.
CREATE INDEX files_filename ON files (tenant_id, collection_id, scope, filename);
CREATE INDEX files_names ON files USING gin (names);

ALTER TABLE documents
    ADD COLUMN published_file uuid REFERENCES files (id) ON DELETE RESTRICT,
    ADD COLUMN draft_file uuid REFERENCES files (id) ON DELETE RESTRICT;
ALTER TABLE versions
    ADD COLUMN file_id uuid REFERENCES files (id) ON DELETE RESTRICT;

CREATE INDEX documents_published_file ON documents (published_file) WHERE published_file IS NOT NULL;
CREATE INDEX documents_draft_file ON documents (draft_file) WHERE draft_file IS NOT NULL;
CREATE INDEX versions_file ON versions (file_id) WHERE file_id IS NOT NULL;
