-- Drafts, versions and publishing.

-- A document with drafts on has two copies, and each keeps its own links: an unpublished
-- draft may link elsewhere than what's live, and both count when something is deleted.
ALTER TABLE relations
    ADD COLUMN copy text NOT NULL DEFAULT 'published' CHECK (copy IN ('published', 'draft')),
    DROP CONSTRAINT relations_pkey,
    ADD PRIMARY KEY (from_doc, copy, field_path);

-- Versions and jobs name their document's collection, which pruning and rewrites select
-- by, so the pair must be the document's own.
ALTER TABLE documents
    ADD CONSTRAINT documents_tenant_collection_id_key UNIQUE (tenant_id, collection_id, id);

-- Past copies of a document, newest first; they go with the document.
CREATE TABLE versions (
    id            uuid PRIMARY KEY,
    tenant_id     uuid NOT NULL REFERENCES tenants (id),
    collection_id uuid NOT NULL,
    document_id   uuid NOT NULL,
    kind          text NOT NULL CHECK (kind IN ('draft', 'published')),
    data          jsonb NOT NULL,
    created_by    uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant_id, collection_id, document_id)
        REFERENCES documents (tenant_id, collection_id, id) ON DELETE CASCADE
);

CREATE INDEX versions_document ON versions (document_id, created_at DESC, id DESC);

-- Work to run later, such as a scheduled publish. A worker leases a due job by setting
-- `locked_until`; one whose lease ran out is taken again.
CREATE TABLE jobs (
    id            uuid PRIMARY KEY,
    tenant_id     uuid NOT NULL REFERENCES tenants (id),
    kind          text NOT NULL,
    document_id   uuid,
    collection_id uuid,
    payload       jsonb NOT NULL DEFAULT '{}',
    run_at        timestamptz NOT NULL,
    state         text NOT NULL CHECK (state IN ('queued', 'running', 'done', 'failed', 'cancelled')),
    attempts      int NOT NULL DEFAULT 0,
    max_attempts  int NOT NULL DEFAULT 5,
    locked_until  timestamptz,
    last_error    text,
    dedupe_key    text,
    created_by    uuid,
    key_id        uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz,
    -- MATCH SIMPLE: a job about no document (document_id NULL) isn't checked. One about a
    -- document must name its collection, or the key would skip the check too.
    FOREIGN KEY (tenant_id, collection_id, document_id)
        REFERENCES documents (tenant_id, collection_id, id) ON DELETE CASCADE,
    CHECK (document_id IS NULL OR collection_id IS NOT NULL)
);

CREATE UNIQUE INDEX jobs_one_pending ON jobs (dedupe_key) WHERE state IN ('queued', 'running');
CREATE INDEX jobs_due ON jobs (run_at) WHERE state = 'queued';
CREATE INDEX jobs_leased ON jobs (locked_until) WHERE state = 'running';
-- Pruning keeps any version a pending job holds.
CREATE INDEX jobs_held_version ON jobs ((payload->>'version')) WHERE state IN ('queued', 'running');

-- Listing a collection's trash, most recently deleted first.
CREATE INDEX documents_trash ON documents (tenant_id, collection_id, deleted_at DESC)
    WHERE deleted_at IS NOT NULL;
