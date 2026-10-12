use serde::Serialize;
use serde_json::{Map, Value};

use crate::schema::Field;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

pub fn validate_document(
    _: &[Field],
    _: &Value,
    _: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>, Vec<FieldError>> {
    unimplemented!("Task 3")
}

pub(crate) fn check_value(f: &Field, v: &Value, path: &str, errors: &mut Vec<FieldError>) {
    if matches!(f.kind, crate::FieldKind::Number { .. }) && !v.is_number() {
        errors.push(FieldError {
            path: path.into(),
            message: "expected a number".into(),
        });
    }
}
