//! Content model and validation for TachyonCMS. This crate does no I/O.

pub mod query;
pub mod schema;
pub mod validate;

pub use schema::{CollectionKind, CollectionSchema, Field, FieldKind, SchemaError};
pub use validate::{
    FieldError, RelationRef, map_relations, normalize_date_time, relations, slugify,
    validate_document,
};
