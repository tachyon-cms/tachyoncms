use serde::Serialize;
use serde_json::{Map, Value};
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use crate::schema::{Field, FieldKind};

const DATE: &[BorrowedFormatItem<'_>] = format_description!("[year]-[month]-[day]");

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldError {
    /// Dotted path to the bad value, such as `seo.title` or `links.0.url`. Empty means the whole document.
    pub path: String,
    pub message: String,
}

/// Validates a document and returns what should be stored.
///
/// With `existing = None` this is a create, and missing fields get their default.
/// With `existing = Some` this is an update: keys in `input` replace existing ones,
/// `null` removes a key, and the merged result is validated as a whole.
pub fn validate_document(
    fields: &[Field],
    input: &Value,
    existing: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>, Vec<FieldError>> {
    let Value::Object(input) = input else {
        return Err(vec![error(String::new(), "expected a JSON object")]);
    };
    let mut doc = existing.cloned().unwrap_or_default();
    for (key, value) in input {
        if value.is_null() {
            doc.remove(key);
        } else {
            doc.insert(key.clone(), value.clone());
        }
    }
    if existing.is_none() {
        for f in fields {
            if let Some(default) = &f.default
                && !doc.contains_key(&f.name)
            {
                doc.insert(f.name.clone(), default.clone());
            }
        }
    }
    let mut errors = Vec::new();
    check_object(fields, &doc, "", &mut errors);
    // Postgres jsonb can't store U+0000, so catch it here as a field error instead of a 500.
    for (key, value) in &doc {
        if key.contains('\0') || contains_nul(value) {
            errors.push(error(key.clone(), "must not contain the NUL character"));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    normalize_dates(fields, &mut doc);
    Ok(doc)
}

fn contains_nul(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(contains_nul),
        Value::Object(obj) => obj.iter().any(|(k, v)| k.contains('\0') || contains_nul(v)),
        _ => false,
    }
}

/// Date-times are stored in UTC, whatever offset the client sent.
fn normalize_dates(fields: &[Field], obj: &mut Map<String, Value>) {
    for f in fields {
        let Some(value) = obj.get_mut(&f.name) else {
            continue;
        };
        match (&f.kind, value) {
            (FieldKind::Date { date_only: false }, Value::String(s)) => {
                if let Ok(t) = time::OffsetDateTime::parse(s, &Rfc3339)
                    && let Ok(utc) = t.to_offset(time::UtcOffset::UTC).format(&Rfc3339)
                {
                    *s = utc;
                }
            }
            (FieldKind::Group { fields }, Value::Object(inner)) => normalize_dates(fields, inner),
            (FieldKind::Array { fields, .. }, Value::Array(rows)) => {
                for row in rows {
                    if let Value::Object(inner) = row {
                        normalize_dates(fields, inner);
                    }
                }
            }
            _ => {}
        }
    }
}

fn check_object(
    fields: &[Field],
    obj: &Map<String, Value>,
    prefix: &str,
    errors: &mut Vec<FieldError>,
) {
    for key in obj.keys() {
        if !fields.iter().any(|f| &f.name == key) {
            errors.push(error(join(prefix, key), "unknown field"));
        }
    }
    for f in fields {
        let path = join(prefix, &f.name);
        match obj.get(&f.name) {
            None | Some(Value::Null) if f.required => errors.push(error(path, "is required")),
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if f.required && s.trim().is_empty() => {
                errors.push(error(path, "is required"))
            }
            Some(v) => check_value(f, v, &path, errors),
        }
    }
}

pub(crate) fn check_value(f: &Field, v: &Value, path: &str, errors: &mut Vec<FieldError>) {
    match (&f.kind, v) {
        (FieldKind::Group { fields }, Value::Object(obj)) => {
            check_object(fields, obj, path, errors)
        }
        (
            FieldKind::Array {
                fields,
                min_rows,
                max_rows,
            },
            Value::Array(rows),
        ) => {
            if let Some(min) = *min_rows
                && rows.len() < min
            {
                errors.push(error(
                    path.into(),
                    &format!("must have at least {}", row_count(min)),
                ));
            }
            if let Some(max) = *max_rows
                && rows.len() > max
            {
                errors.push(error(
                    path.into(),
                    &format!("must have at most {}", row_count(max)),
                ));
            }
            for (i, row) in rows.iter().enumerate() {
                let row_path = format!("{path}.{i}");
                match row {
                    Value::Object(obj) => check_object(fields, obj, &row_path, errors),
                    _ => errors.push(error(row_path, "expected an object")),
                }
            }
        }
        _ => {
            if let Some(message) = scalar_error(&f.kind, v) {
                errors.push(error(path.into(), &message));
            }
        }
    }
}

fn scalar_error(kind: &FieldKind, v: &Value) -> Option<String> {
    match kind {
        FieldKind::Text {
            min_length,
            max_length,
        } => text_error(v, *min_length, *max_length),
        FieldKind::Textarea { max_length } => text_error(v, None, *max_length),
        FieldKind::Number { min, max, integer } => {
            let Some(n) = v.as_f64() else {
                return Some("expected a number".into());
            };
            if *integer && n.fract() != 0.0 {
                return Some("expected a whole number".into());
            }
            if let Some(min) = min
                && n < *min
            {
                return Some(format!("must be at least {min}"));
            }
            if let Some(max) = max
                && n > *max
            {
                return Some(format!("must be at most {max}"));
            }
            None
        }
        FieldKind::Boolean => (!v.is_boolean()).then(|| "expected true or false".into()),
        FieldKind::Date { date_only: true } => {
            let ok = v
                .as_str()
                .is_some_and(|s| time::Date::parse(s, DATE).is_ok());
            (!ok).then(|| "expected a date like 2026-10-12".into())
        }
        FieldKind::Date { date_only: false } => {
            let ok = v
                .as_str()
                .is_some_and(|s| time::OffsetDateTime::parse(s, &Rfc3339).is_ok());
            (!ok).then(|| "expected a date-time like 2026-10-12T09:30:00Z".into())
        }
        FieldKind::Email => {
            (!v.as_str().is_some_and(is_email)).then(|| "expected an email address".into())
        }
        FieldKind::Url => {
            (!v.as_str().is_some_and(is_web_url)).then(|| "expected an http or https URL".into())
        }
        FieldKind::Select { options, multiple } => {
            let allowed = |x: &Value| x.as_str().is_some_and(|s| options.iter().any(|o| o == s));
            let list = options.join(", ");
            if *multiple {
                let ok = v.as_array().is_some_and(|xs| xs.iter().all(allowed));
                (!ok).then(|| format!("expected a list of: {list}"))
            } else {
                (!allowed(v)).then(|| format!("expected one of: {list}"))
            }
        }
        FieldKind::Slug { .. } | FieldKind::Relationship { .. } => None,
        FieldKind::Json => None,
        FieldKind::Group { .. } => Some("expected an object".into()),
        FieldKind::Array { .. } => Some("expected a list".into()),
    }
}

fn text_error(v: &Value, min: Option<usize>, max: Option<usize>) -> Option<String> {
    let Value::String(s) = v else {
        return Some("expected text".into());
    };
    // Characters, not bytes: "café" is 4 long.
    let len = s.chars().count();
    if let Some(min) = min
        && len < min
    {
        return Some(format!("must be at least {min} characters"));
    }
    if let Some(max) = max
        && len > max
    {
        return Some(format!("must be at most {max} characters"));
    }
    None
}

fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.rsplit_once('@') else {
        return false;
    };
    s.len() <= 254
        && !local.is_empty()
        && !local.contains('@')
        && !s.chars().any(char::is_whitespace)
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

fn is_web_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
}

fn row_count(n: usize) -> String {
    format!("{n} row{}", if n == 1 { "" } else { "s" })
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn error(path: String, message: &str) -> FieldError {
    FieldError {
        path,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(v: Value) -> Vec<Field> {
        serde_json::from_value(v).expect("fields JSON should parse")
    }

    fn ok(fields: &[Field], input: Value) -> Map<String, Value> {
        validate_document(fields, &input, None).expect("should be valid")
    }

    fn errs(fields: &[Field], input: Value) -> Vec<(String, String)> {
        validate_document(fields, &input, None)
            .expect_err("should be invalid")
            .into_iter()
            .map(|e| (e.path, e.message))
            .collect()
    }

    fn e(path: &str, message: &str) -> (String, String) {
        (path.into(), message.into())
    }

    #[test]
    fn applies_defaults_on_create() {
        let f = fields(json!([
            {"name": "title", "type": "text", "required": true},
            {"name": "featured", "type": "boolean", "default": false}
        ]));
        assert_eq!(
            Value::Object(ok(&f, json!({"title": "Hello"}))),
            json!({"title": "Hello", "featured": false})
        );
    }

    #[test]
    fn required_fields_must_be_present_and_not_blank() {
        let f = fields(json!([{"name": "title", "type": "text", "required": true}]));
        assert_eq!(errs(&f, json!({})), vec![e("title", "is required")]);
        assert_eq!(
            errs(&f, json!({"title": "   "})),
            vec![e("title", "is required")]
        );
        assert_eq!(
            errs(&f, json!({"title": null})),
            vec![e("title", "is required")]
        );
    }

    #[test]
    fn rejects_unknown_fields_and_non_objects() {
        let f = fields(json!([{"name": "title", "type": "text"}]));
        assert_eq!(
            errs(&f, json!({"title": "x", "extra": 1})),
            vec![e("extra", "unknown field")]
        );
        assert_eq!(
            errs(&f, json!(["title"])),
            vec![e("", "expected a JSON object")]
        );
    }

    #[test]
    fn counts_text_length_in_characters_not_bytes() {
        let f = fields(json!([{"name": "t", "type": "text", "minLength": 2, "maxLength": 4}]));
        ok(&f, json!({"t": "café"})); // 4 characters, 5 bytes
        assert_eq!(
            errs(&f, json!({"t": "cafés"})),
            vec![e("t", "must be at most 4 characters")]
        );
        assert_eq!(
            errs(&f, json!({"t": "é"})),
            vec![e("t", "must be at least 2 characters")]
        );
        assert_eq!(errs(&f, json!({"t": 5})), vec![e("t", "expected text")]);
    }

    #[test]
    fn numbers() {
        let f =
            fields(json!([{"name": "n", "type": "number", "integer": true, "min": 0, "max": 10}]));
        ok(&f, json!({"n": 3}));
        assert_eq!(
            errs(&f, json!({"n": 3.5})),
            vec![e("n", "expected a whole number")]
        );
        assert_eq!(
            errs(&f, json!({"n": -1})),
            vec![e("n", "must be at least 0")]
        );
        assert_eq!(
            errs(&f, json!({"n": 11})),
            vec![e("n", "must be at most 10")]
        );
        assert_eq!(
            errs(&f, json!({"n": "3"})),
            vec![e("n", "expected a number")]
        );
    }

    #[test]
    fn booleans() {
        let f = fields(json!([{"name": "b", "type": "boolean"}]));
        ok(&f, json!({"b": true}));
        assert_eq!(
            errs(&f, json!({"b": "yes"})),
            vec![e("b", "expected true or false")]
        );
    }

    #[test]
    fn dates() {
        let f = fields(json!([
            {"name": "day", "type": "date", "dateOnly": true},
            {"name": "at", "type": "date"}
        ]));
        ok(
            &f,
            json!({"day": "2026-10-12", "at": "2026-10-12T09:30:00Z"}),
        );
        assert_eq!(
            errs(&f, json!({"day": "2026-13-01"})),
            vec![e("day", "expected a date like 2026-10-12")]
        );
        assert_eq!(
            errs(&f, json!({"at": "2026-10-12"})),
            vec![e("at", "expected a date-time like 2026-10-12T09:30:00Z")]
        );
    }

    #[test]
    fn emails_and_urls() {
        let f =
            fields(json!([{"name": "email", "type": "email"}, {"name": "site", "type": "url"}]));
        ok(
            &f,
            json!({"email": "jo@example.com", "site": "https://example.com/a"}),
        );
        for bad in [
            "jo",
            "jo@",
            "@example.com",
            "jo@example",
            "jo @example.com",
            "jo@.example.com",
        ] {
            assert_eq!(
                errs(&f, json!({"email": bad})),
                vec![e("email", "expected an email address")],
                "{bad}"
            );
        }
        for bad in [
            "example.com",
            "javascript:alert(1)",
            "ftp://example.com",
            "https://",
        ] {
            assert_eq!(
                errs(&f, json!({"site": bad})),
                vec![e("site", "expected an http or https URL")],
                "{bad}"
            );
        }
    }

    #[test]
    fn selects() {
        let f = fields(json!([
            {"name": "one", "type": "select", "options": ["a", "b"]},
            {"name": "many", "type": "select", "options": ["a", "b"], "multiple": true}
        ]));
        ok(&f, json!({"one": "a", "many": ["a", "b"]}));
        assert_eq!(
            errs(&f, json!({"one": "c"})),
            vec![e("one", "expected one of: a, b")]
        );
        assert_eq!(
            errs(&f, json!({"many": "a"})),
            vec![e("many", "expected a list of: a, b")]
        );
        assert_eq!(
            errs(&f, json!({"many": ["a", "z"]})),
            vec![e("many", "expected a list of: a, b")]
        );
    }

    #[test]
    fn nested_errors_carry_their_path() {
        let f = fields(json!([
            {"name": "seo", "type": "group", "fields": [{"name": "title", "type": "text", "required": true}]},
            {"name": "links", "type": "array", "minRows": 1, "maxRows": 2, "fields": [{"name": "url", "type": "url"}]}
        ]));
        assert_eq!(
            errs(&f, json!({"seo": {}, "links": [{"url": "nope"}, 3]})),
            vec![
                e("seo.title", "is required"),
                e("links.0.url", "expected an http or https URL"),
                e("links.1", "expected an object")
            ]
        );
        assert_eq!(
            errs(&f, json!({"seo": {"title": "x"}, "links": []})),
            vec![e("links", "must have at least 1 row")]
        );
        assert_eq!(
            errs(&f, json!({"seo": {"title": "x"}, "links": [{}, {}, {}]})),
            vec![e("links", "must have at most 2 rows")]
        );
        assert_eq!(
            errs(&f, json!({"seo": "x"})),
            vec![e("seo", "expected an object")]
        );
    }

    #[test]
    fn rejects_nul_characters_anywhere() {
        let f = fields(json!([{"name": "t", "type": "text"}, {"name": "data", "type": "json"}]));
        assert_eq!(
            errs(&f, json!({"t": "a\u{0}b"})),
            vec![e("t", "must not contain the NUL character")]
        );
        assert_eq!(
            errs(&f, json!({"data": {"k\u{0}": ["x\u{0}"]}})),
            vec![e("data", "must not contain the NUL character")]
        );
    }

    #[test]
    fn date_times_are_stored_in_utc() {
        let f = fields(json!([
            {"name": "at", "type": "date"},
            {"name": "times", "type": "array", "fields": [{"name": "at", "type": "date"}]}
        ]));
        let doc = ok(
            &f,
            json!({"at": "2026-10-12T09:30:00+02:00", "times": [{"at": "2026-10-12T00:15:00-01:00"}]}),
        );
        assert_eq!(
            Value::Object(doc),
            json!({"at": "2026-10-12T07:30:00Z", "times": [{"at": "2026-10-12T01:15:00Z"}]})
        );
    }

    #[test]
    fn update_merges_and_null_removes() {
        let f = fields(json!([
            {"name": "title", "type": "text", "required": true},
            {"name": "summary", "type": "textarea"},
            {"name": "featured", "type": "boolean", "default": false}
        ]));
        let existing = json!({"title": "Old", "summary": "S"})
            .as_object()
            .unwrap()
            .clone();
        let merged = validate_document(
            &f,
            &json!({"title": "New", "summary": null}),
            Some(&existing),
        )
        .unwrap();
        // `featured` stays absent: defaults only apply on create.
        assert_eq!(Value::Object(merged), json!({"title": "New"}));
        let err = validate_document(&f, &json!({"title": null}), Some(&existing)).unwrap_err();
        assert_eq!(
            err,
            vec![FieldError {
                path: "title".into(),
                message: "is required".into()
            }]
        );
    }
}
