//! Content model and validation for TachyonCMS. This crate does no I/O.

pub mod schema;
pub mod validate;

pub use schema::{CollectionKind, CollectionSchema, Field, FieldKind, SchemaError};
pub use validate::{FieldError, validate_document};
