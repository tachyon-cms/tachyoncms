-- Links between documents, mirrored from relationship fields so we can find who links
-- to whom without scanning JSON, and refuse to delete a document others depend on.
CREATE TABLE relations (
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    from_doc   uuid NOT NULL REFERENCES documents (id) ON DELETE CASCADE,
    field_path text NOT NULL,
    to_doc     uuid NOT NULL REFERENCES documents (id),
    PRIMARY KEY (from_doc, field_path)
);

CREATE INDEX relations_to_doc ON relations (to_doc);
