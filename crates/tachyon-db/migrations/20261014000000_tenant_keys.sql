-- Every row's links stay inside its own tenant: a document can only sit in a collection of
-- the same tenant, and a link can only join documents of the same tenant. Row-level
-- security will rely on this once there is more than one tenant.
ALTER TABLE collections ADD CONSTRAINT collections_tenant_id_id_key UNIQUE (tenant_id, id);
ALTER TABLE documents ADD CONSTRAINT documents_tenant_id_id_key UNIQUE (tenant_id, id);

ALTER TABLE documents
    DROP CONSTRAINT documents_collection_id_fkey,
    ADD CONSTRAINT documents_collection_fkey FOREIGN KEY (tenant_id, collection_id)
        REFERENCES collections (tenant_id, id) ON DELETE CASCADE;

ALTER TABLE relations
    DROP CONSTRAINT relations_from_doc_fkey,
    DROP CONSTRAINT relations_to_doc_fkey,
    ADD CONSTRAINT relations_from_doc_fkey FOREIGN KEY (tenant_id, from_doc)
        REFERENCES documents (tenant_id, id) ON DELETE CASCADE,
    ADD CONSTRAINT relations_to_doc_fkey FOREIGN KEY (tenant_id, to_doc)
        REFERENCES documents (tenant_id, id);
