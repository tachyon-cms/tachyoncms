-- Join fields look up "who links to this document" and only need the linking document's
-- ID, so carrying it in the index lets that lookup skip the table. It also serves the
-- foreign key on to_doc, so the narrower index goes.
CREATE INDEX relations_to_doc_from_doc ON relations (to_doc, from_doc);
DROP INDEX relations_to_doc;
