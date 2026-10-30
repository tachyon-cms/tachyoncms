-- Shared blocks: block types that `blocks` fields name with {"blockRef": "<slug>"}. They're
-- stored with collections and globals, so one slug can't name two of them, but hold no documents.

ALTER TABLE collections DROP CONSTRAINT collections_kind_check;
ALTER TABLE collections
    ADD CONSTRAINT collections_kind_check CHECK (kind IN ('collection', 'global', 'block'));
