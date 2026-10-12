CREATE TABLE tenants (
    id         uuid PRIMARY KEY,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- One tenant until multi-tenancy ships. Matches DEFAULT_TENANT in src/lib.rs.
INSERT INTO tenants (id, name) VALUES ('00000000-0000-0000-0000-000000000001', 'default');

CREATE TABLE collections (
    id             uuid PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES tenants (id),
    slug           text NOT NULL,
    kind           text NOT NULL CHECK (kind IN ('collection', 'global')),
    schema         jsonb NOT NULL,
    schema_version integer NOT NULL DEFAULT 1,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, slug)
);

CREATE TABLE documents (
    id            uuid PRIMARY KEY,
    tenant_id     uuid NOT NULL REFERENCES tenants (id),
    collection_id uuid NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    status        text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'published', 'scheduled')),
    published     jsonb,
    draft         jsonb,
    published_at  timestamptz,
    scheduled_at  timestamptz,
    created_by    uuid,
    updated_by    uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    deleted_at    timestamptz
);

-- Listing a collection newest-first (UUIDv7 sorts by time).
CREATE INDEX documents_live ON documents (tenant_id, collection_id, id DESC) WHERE deleted_at IS NULL;
CREATE INDEX documents_published ON documents USING gin (published jsonb_path_ops) WHERE deleted_at IS NULL;
