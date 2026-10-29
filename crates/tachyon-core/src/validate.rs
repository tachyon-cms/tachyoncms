use serde::Serialize;
use serde_json::{Map, Value};
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use uuid::Uuid;

use crate::schema::{
    BLOCK_ROW_KEYS, BlockSource, Field, FieldKind, RelationTo, SelectOption, data_fields,
};

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
/// With `existing = Some` this is an update: groups merge key by key, everything else
/// (lists, rows, `json` values) is replaced, and the merged result is validated as a whole.
/// Either way `null` means "no value" at any depth, except inside `json` fields, where it's data.
pub fn validate_document(
    fields: &[Field],
    input: &Value,
    existing: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>, Vec<FieldError>> {
    let fields: &[Field] = &data_fields(fields);
    let Value::Object(input) = input else {
        return Err(vec![error(String::new(), "expected a JSON object")]);
    };
    let mut doc = existing.cloned().unwrap_or_default();
    merge(fields, &mut doc, input);
    if existing.is_none() {
        apply_defaults(fields, &mut doc);
        for f in fields {
            if let FieldKind::Slug { from: Some(source) } = &f.kind
                && !doc.contains_key(&f.name)
                && let Some(Value::String(text)) = doc.get(source)
            {
                let slug = slugify(text);
                if !slug.is_empty() {
                    doc.insert(f.name.clone(), Value::String(slug));
                }
            }
        }
    }
    let mut errors = Vec::new();
    check_object(fields, &doc, &[], "", &mut errors, None);
    // Postgres jsonb can't store U+0000, so catch it here as a field error instead of a 500.
    for (key, value) in &doc {
        if key.contains('\0') || contains_nul(value) {
            errors.push(error(key.clone(), "must not contain the NUL character"));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    normalize(fields, &mut doc);
    Ok(doc)
}

/// The document `validate_document` checks, before defaults: `existing` with `input`
/// written over it. The row indices in its error paths count rows of this.
pub fn merged(
    fields: &[Field],
    input: &Value,
    existing: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut doc = existing.cloned().unwrap_or_default();
    if let Value::Object(input) = input {
        merge(&data_fields(fields), &mut doc, input);
    }
    doc
}

/// Writes `input` over `doc`: `null` removes a key, groups merge, anything else replaces.
fn merge(fields: &[Field], doc: &mut Map<String, Value>, input: &Map<String, Value>) {
    for (key, value) in input {
        let field = fields.iter().find(|f| &f.name == key);
        match (field.map(|f| &f.kind), value, doc.get_mut(key)) {
            (_, Value::Null, _) => {
                doc.remove(key);
            }
            (
                Some(FieldKind::Group { fields }),
                Value::Object(patch),
                Some(Value::Object(current)),
            ) => merge(fields, current, patch),
            _ => {
                let mut value = value.clone();
                if let Some(f) = field {
                    strip_nulls(f, &mut value);
                    // What replaces the stored value whole (rows, or a group the document
                    // didn't have) is new, so it gets its defaults.
                    match (&f.kind, &mut value) {
                        (
                            FieldKind::Array { .. } | FieldKind::Blocks { .. },
                            Value::Array(rows),
                        ) => {
                            for row in rows {
                                if let Value::Object(row) = row {
                                    apply_defaults(row_fields(&f.kind, row), row);
                                }
                            }
                        }
                        // Including fields the caller may not update: a group that wasn't
                        // stored never held them, so its defaults overwrite nothing.
                        (FieldKind::Group { fields }, Value::Object(group)) => {
                            apply_defaults(fields, group)
                        }
                        (FieldKind::RichText { .. }, text) => block_defaults(&f.kind, text),
                        _ => {}
                    }
                }
                doc.insert(key.clone(), value);
            }
        }
    }
}

/// Drops `null` keys inside groups and array rows. A `json` field's nulls are its own data.
fn strip_nulls(f: &Field, value: &mut Value) {
    match (&f.kind, value) {
        (FieldKind::Group { fields }, Value::Object(obj)) => strip_object(fields, obj),
        (kind @ (FieldKind::Array { .. } | FieldKind::Blocks { .. }), Value::Array(rows)) => {
            for row in rows {
                if let Value::Object(obj) = row {
                    strip_object(row_fields(kind, obj), obj);
                }
            }
        }
        (kind @ FieldKind::RichText { .. }, text) => {
            crate::richtext::blocks_mut(text, &mut |attrs| {
                let fields = node_fields(kind, attrs);
                if let Some(Value::Object(obj)) = attrs.get_mut("fields") {
                    strip_object(fields, obj);
                }
            })
        }
        _ => {}
    }
}

/// The fields of the block type a rich text block node's `attrs` name; none when unknown.
pub(crate) fn node_fields<'a>(kind: &'a FieldKind, attrs: &Map<String, Value>) -> &'a [Field] {
    attrs
        .get("blockType")
        .and_then(Value::as_str)
        .and_then(|slug| kind.block(slug))
        .map_or(&[], |b| &b.fields)
}

/// Fills defaults into the fields of every block node in rich text `text`.
fn block_defaults(kind: &FieldKind, text: &mut Value) {
    crate::richtext::blocks_mut(text, &mut |attrs| {
        let fields = node_fields(kind, attrs);
        match attrs.get_mut("fields") {
            Some(Value::Object(obj)) => apply_defaults(fields, obj),
            None => {
                let filled = defaults(fields);
                if !filled.is_empty() {
                    attrs.insert("fields".into(), Value::Object(filled));
                }
            }
            Some(_) => {}
        }
    })
}

fn strip_object(fields: &[Field], obj: &mut Map<String, Value>) {
    obj.retain(|_, v| !v.is_null());
    for f in fields {
        if let Some(v) = obj.get_mut(&f.name) {
            strip_nulls(f, v);
        }
    }
}

/// Fills missing values that have a default, inside groups and array rows too. A missing
/// group is created when something inside it has a default.
pub(crate) fn apply_defaults(fields: &[Field], obj: &mut Map<String, Value>) {
    for f in fields {
        match (&f.kind, obj.get_mut(&f.name)) {
            (FieldKind::Group { fields }, None) if f.default.is_some() => {
                let mut group = f.default.clone().unwrap_or_default();
                if let Value::Object(inner) = &mut group {
                    apply_defaults(fields, inner);
                }
                obj.insert(f.name.clone(), group);
            }
            (_, None) if f.default.is_some() => {
                obj.insert(f.name.clone(), f.default.clone().unwrap_or_default());
            }
            (FieldKind::Group { fields }, None) => {
                let inner = defaults(fields);
                if !inner.is_empty() {
                    obj.insert(f.name.clone(), Value::Object(inner));
                }
            }
            (FieldKind::Group { fields }, Some(Value::Object(inner))) => {
                apply_defaults(fields, inner)
            }
            (
                kind @ (FieldKind::Array { .. } | FieldKind::Blocks { .. }),
                Some(Value::Array(rows)),
            ) => {
                for row in rows {
                    if let Value::Object(row) = row {
                        apply_defaults(row_fields(kind, row), row);
                    }
                }
            }
            (kind @ FieldKind::RichText { .. }, Some(text)) => block_defaults(kind, text),
            _ => {}
        }
    }
}

/// What a new document holds before anyone types anything: every default, nested ones included.
pub fn defaults(fields: &[Field]) -> Map<String, Value> {
    let mut out = Map::new();
    apply_defaults(&data_fields(fields), &mut out);
    out
}

fn contains_nul(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(contains_nul),
        Value::Object(obj) => obj.iter().any(|(k, v)| k.contains('\0') || contains_nul(v)),
        _ => false,
    }
}

/// Stores date-times in UTC and relationship IDs in canonical form, whatever the client sent.
fn normalize(fields: &[Field], obj: &mut Map<String, Value>) {
    for f in fields {
        let Some(value) = obj.get_mut(&f.name) else {
            continue;
        };
        match (&f.kind, value) {
            (FieldKind::Date { date_only: false }, Value::String(s)) => {
                if let Some(utc) = normalize_date_time(s) {
                    *s = utc;
                }
            }
            (
                FieldKind::Relationship {
                    has_many: false, ..
                },
                link,
            ) => canonical_link(link),
            (FieldKind::Relationship { has_many: true, .. }, Value::Array(links)) => {
                links.iter_mut().for_each(canonical_link);
            }
            (FieldKind::Group { fields }, Value::Object(inner)) => normalize(fields, inner),
            (FieldKind::Array { fields, .. }, Value::Array(rows)) => {
                for row in rows {
                    if let Value::Object(inner) = row {
                        normalize(fields, inner);
                    }
                }
            }
            // A block row keeps the id it was sent with, so editors can tell rows apart
            // across saves; a new row gets one.
            (kind @ FieldKind::Blocks { .. }, Value::Array(rows)) => {
                for row in rows {
                    if let Value::Object(inner) = row {
                        normalize(row_fields(kind, inner), inner);
                        keep_or_give_id(inner);
                    }
                }
            }
            // Block nodes get IDs as block rows do; links are stored in canonical form.
            (kind @ FieldKind::RichText { .. }, text) => {
                crate::richtext::lower_schemes(text);
                crate::richtext::each_item_mut(text, &mut |_, item| match item {
                    crate::richtext::ItemMut::Link(link) => canonical_link(link),
                    crate::richtext::ItemMut::Block(attrs) => {
                        let fields = node_fields(kind, attrs);
                        if let Some(Value::Object(obj)) = attrs.get_mut("fields") {
                            normalize(fields, obj);
                        }
                        keep_or_give_id(attrs);
                    }
                })
            }
            _ => {}
        }
    }
}

/// Canonicalises the `id` of a block row or node, or gives it a new one.
fn keep_or_give_id(obj: &mut Map<String, Value>) {
    match obj.get_mut("id") {
        Some(Value::String(id)) => canonical_id(id),
        _ => {
            obj.insert("id".into(), Uuid::now_v7().to_string().into());
        }
    }
}

/// The fields a list row holds: an array's, or those of the block type the row names.
/// Nothing for a row whose block type is unknown, which validation reports.
pub(crate) fn row_fields<'a>(kind: &'a FieldKind, row: &Map<String, Value>) -> &'a [Field] {
    match kind {
        FieldKind::Array { fields, .. } => fields,
        FieldKind::Blocks { .. } => row
            .get("blockType")
            .and_then(Value::as_str)
            .and_then(|slug| kind.block(slug))
            .map_or(&[], |b| &b.fields),
        _ => &[],
    }
}

/// `own` are keys the object carries besides its fields, such as a block row's `id`.
/// `within`: the limits used so far by the rich text whose block node this is in.
pub(crate) fn check_object(
    fields: &[Field],
    obj: &Map<String, Value>,
    own: &[&str],
    prefix: &str,
    errors: &mut Vec<FieldError>,
    mut within: Option<&mut crate::richtext::Budget>,
) {
    for key in obj.keys() {
        if !fields.iter().any(|f| &f.name == key) && !own.contains(&key.as_str()) {
            errors.push(error(join(prefix, key), "unknown field"));
        }
    }
    for f in fields {
        let path = join(prefix, &f.name);
        if let FieldKind::Join { collection, on, .. } = &f.kind {
            if obj.contains_key(&f.name) {
                errors.push(error(
                    path,
                    &format!("is filled from {collection}.{on}; it can't be set"),
                ));
            }
            continue;
        }
        if let Some(source) = &f.virtual_path {
            if obj.contains_key(&f.name) {
                errors.push(error(
                    path,
                    &format!("is read from {source}; set that instead"),
                ));
            }
            continue;
        }
        match obj.get(&f.name) {
            None | Some(Value::Null) if f.required => errors.push(error(path, "is required")),
            None | Some(Value::Null) => {}
            // Only a well-formed document can be told it's empty.
            Some(text) if f.required && matches!(f.kind, FieldKind::RichText { .. }) => {
                let before = errors.len();
                check_value(f, text, &path, errors, within.as_deref_mut());
                if errors.len() == before && crate::richtext::is_empty(text) {
                    errors.push(error(path, "is required"))
                }
            }
            Some(Value::String(s)) if f.required && s.trim().is_empty() => {
                errors.push(error(path, "is required"))
            }
            Some(Value::Array(items)) if f.required && items.is_empty() && is_list(&f.kind) => {
                errors.push(error(path, "is required"))
            }
            Some(v) => check_value(f, v, &path, errors, within.as_deref_mut()),
        }
    }
}

/// Fields whose value is a list, where an empty list means nothing was chosen.
fn is_list(kind: &FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Text { has_many: true, .. }
            | FieldKind::Number { has_many: true, .. }
            | FieldKind::Relationship { has_many: true, .. }
            | FieldKind::Select { multiple: true, .. }
            | FieldKind::Array { .. }
            | FieldKind::Blocks { .. }
    )
}

/// `within`: as `check_object` takes it.
pub(crate) fn check_value(
    f: &Field,
    v: &Value,
    path: &str,
    errors: &mut Vec<FieldError>,
    mut within: Option<&mut crate::richtext::Budget>,
) {
    match (&f.kind, v) {
        (
            FieldKind::Text {
                has_many: true,
                min_rows,
                max_rows,
                ..
            }
            | FieldKind::Number {
                has_many: true,
                min_rows,
                max_rows,
                ..
            },
            value,
        ) => {
            let Value::Array(items) = value else {
                errors.push(error(path.into(), "expected a list"));
                return;
            };
            check_row_count(items.len(), *min_rows, *max_rows, path, errors);
            // `scalar_error` ignores `has_many`, so it checks one element.
            for (i, item) in items.iter().enumerate() {
                if let Some(message) = scalar_error(&f.kind, item) {
                    errors.push(error(format!("{path}.{i}"), &message));
                }
            }
        }
        (FieldKind::Group { fields }, Value::Object(obj)) => {
            check_object(fields, obj, &[], path, errors, within)
        }
        (
            FieldKind::Array {
                fields,
                min_rows,
                max_rows,
            },
            Value::Array(rows),
        ) => {
            check_row_count(rows.len(), *min_rows, *max_rows, path, errors);
            for (i, row) in rows.iter().enumerate() {
                let row_path = format!("{path}.{i}");
                match row {
                    Value::Object(obj) => {
                        check_object(fields, obj, &[], &row_path, errors, within.as_deref_mut())
                    }
                    _ => errors.push(error(row_path, "expected an object")),
                }
            }
        }
        (
            FieldKind::Blocks {
                blocks,
                min_rows,
                max_rows,
            },
            Value::Array(rows),
        ) => {
            let limit = |n: &Option<u32>| n.map(|n| n as usize);
            check_row_count(rows.len(), limit(min_rows), limit(max_rows), path, errors);
            let mut seen = std::collections::HashSet::new();
            for (i, row) in rows.iter().enumerate() {
                let row_path = format!("{path}.{i}");
                let Value::Object(obj) = row else {
                    errors.push(error(row_path, "expected an object"));
                    continue;
                };
                match obj.get("id").map(|id| id.as_str().map(Uuid::try_parse)) {
                    None => {}
                    Some(Some(Ok(id))) => {
                        // Compared as UUIDs, so the same ID in another case still counts.
                        if !seen.insert(id) {
                            errors
                                .push(error(format!("{row_path}.id"), "this row ID is used twice"));
                        }
                    }
                    Some(_) => errors.push(error(format!("{row_path}.id"), "expected a row ID")),
                }
                let block = match obj.get("blockType") {
                    None | Some(Value::Null) => {
                        errors.push(error(format!("{row_path}.blockType"), "is required"));
                        continue;
                    }
                    Some(t) => match t.as_str().and_then(|slug| f.kind.block(slug)) {
                        Some(block) => block,
                        None => {
                            let slugs: Vec<&str> = blocks
                                .iter()
                                .filter_map(BlockSource::def)
                                .map(|b| b.slug.as_str())
                                .collect();
                            errors.push(error(
                                format!("{row_path}.blockType"),
                                &format!("expected one of: {}", slugs.join(", ")),
                            ));
                            continue;
                        }
                    },
                };
                check_object(
                    &block.fields,
                    obj,
                    BLOCK_ROW_KEYS,
                    &row_path,
                    errors,
                    within.as_deref_mut(),
                );
            }
        }
        (FieldKind::RichText { .. }, text) => {
            let checked = match within {
                Some(budget) => crate::richtext::validate_within(text, &f.kind, path, budget),
                None => crate::richtext::validate(text, &f.kind, path),
            };
            if let Err(e) = checked {
                errors.extend(e);
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
            ..
        } => text_error(v, *min_length, *max_length),
        FieldKind::Textarea {
            min_length,
            max_length,
        } => text_error(v, *min_length, *max_length),
        FieldKind::Code { .. } => text_error(v, None, None),
        FieldKind::Point => {
            let ok = v.as_array().is_some_and(|xy| {
                matches!(xy.as_slice(), [x, y]
                    if x.as_f64().is_some_and(|lng| (-180.0..=180.0).contains(&lng))
                    && y.as_f64().is_some_and(|lat| (-90.0..=90.0).contains(&lat)))
            });
            (!ok).then(|| "expected [longitude, latitude], like [-0.1276, 51.5072]".into())
        }
        FieldKind::Number {
            min, max, integer, ..
        } => {
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
            let ok = v.as_str().is_some_and(is_date);
            (!ok).then(|| "expected a date like 2026-10-12".into())
        }
        FieldKind::Date { date_only: false } => {
            let ok = v.as_str().is_some_and(|s| normalize_date_time(s).is_some());
            (!ok).then(|| "expected a date-time like 2026-10-12T09:30:00Z".into())
        }
        FieldKind::Email => {
            (!v.as_str().is_some_and(is_email)).then(|| "expected an email address".into())
        }
        FieldKind::Url => {
            (!v.as_str().is_some_and(is_web_url)).then(|| "expected an http or https URL".into())
        }
        FieldKind::Radio { options } => {
            let ok = v
                .as_str()
                .is_some_and(|s| options.iter().any(|o| o.value() == s));
            (!ok).then(|| format!("expected one of: {}", values_list(options)))
        }
        FieldKind::Select { options, multiple } => {
            let allowed = |x: &Value| {
                x.as_str()
                    .is_some_and(|s| options.iter().any(|o| o.value() == s))
            };
            let list = values_list(options);
            if *multiple {
                let ok = v.as_array().is_some_and(|xs| xs.iter().all(allowed));
                (!ok).then(|| format!("expected a list of: {list}"))
            } else {
                (!allowed(v)).then(|| format!("expected one of: {list}"))
            }
        }
        FieldKind::Slug { .. } => (!v.as_str().is_some_and(is_slug)).then(|| {
            "expected lowercase letters, digits and single hyphens, like my-first-post".into()
        }),
        FieldKind::Relationship {
            relation_to: RelationTo::One(_),
            has_many: false, .. } => (!v.as_str().is_some_and(is_uuid)).then(|| "expected a document ID".into()),
        FieldKind::Relationship {
            relation_to: RelationTo::One(_),
            has_many: true, .. } => {
            let ok = v
                .as_array()
                .is_some_and(|ids| ids.iter().all(|x| x.as_str().is_some_and(is_uuid)));
            (!ok).then(|| "expected a list of document IDs".into())
        }
        FieldKind::Relationship {
            relation_to,
            has_many: false, .. } => (!is_link(v, relation_to)).then(|| {
            format!(
                "expected a link like {{\"relationTo\": \"<collection>\", \"value\": \"<document ID>\"}} to one of: {relation_to}"
            )
        }),
        FieldKind::Relationship {
            relation_to,
            has_many: true, .. } => {
            let ok = v
                .as_array()
                .is_some_and(|links| links.iter().all(|l| is_link(l, relation_to)));
            (!ok).then(|| {
                format!(
                    "expected a list of links like {{\"relationTo\": \"<collection>\", \"value\": \"<document ID>\"}} to: {relation_to}"
                )
            })
        }
        FieldKind::Json => None,
        // Never reached: `check_value` hands rich text to `richtext::validate`.
        FieldKind::RichText { .. } => None,
        // Never reached: `check_object` refuses a join value before this.
        FieldKind::Join { .. } => None,
        // Never reached: validation only sees the data shape, where layout has dissolved.
        FieldKind::Row { .. } | FieldKind::Collapsible { .. } | FieldKind::Tabs { .. } => None,
        FieldKind::Group { .. } => Some("expected an object".into()),
        FieldKind::Array { .. } | FieldKind::Blocks { .. } => Some("expected a list".into()),
    }
}

fn values_list(options: &[SelectOption]) -> String {
    options
        .iter()
        .map(SelectOption::value)
        .collect::<Vec<_>>()
        .join(", ")
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

/// A plausible email address: one `@`, a dotted domain, no spaces, at most 254 characters.
pub fn is_email(s: &str) -> bool {
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

const DATE_TIME_UTC: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Parses an RFC 3339 date-time and returns it in UTC with exactly three fractional digits,
/// so stored values sort correctly as text.
/// Years outside 0–9999 are rejected: they'd break fixed-width text ordering.
pub fn normalize_date_time(s: &str) -> Option<String> {
    let utc = time::OffsetDateTime::parse(s, &Rfc3339)
        .ok()?
        .checked_to_offset(time::UtcOffset::UTC)?;
    if !(0..=9999).contains(&utc.year()) {
        return None;
    }
    utc.format(DATE_TIME_UTC).ok()
}

pub(crate) fn is_date(s: &str) -> bool {
    time::Date::parse(s, DATE).is_ok()
}

/// Turns "Hello, World! Café" into "hello-world-café".
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn is_slug(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && slugify(s) == s
}

fn is_uuid(s: &str) -> bool {
    Uuid::try_parse(s).is_ok()
}

fn canonical_id(s: &mut String) {
    if let Ok(id) = Uuid::try_parse(s) {
        *s = id.to_string();
    }
}

fn is_web_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
}

fn check_row_count(
    len: usize,
    min: Option<usize>,
    max: Option<usize>,
    path: &str,
    errors: &mut Vec<FieldError>,
) {
    if let Some(min) = min
        && len < min
    {
        errors.push(error(
            path.into(),
            &format!("must have at least {}", row_count(min)),
        ));
    }
    if let Some(max) = max
        && len > max
    {
        errors.push(error(
            path.into(),
            &format!("must have at most {}", row_count(max)),
        ));
    }
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

/// A link from a document to another document.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationRef {
    /// Where the ID sits in the document: `author`, `related.1`, `links.0.author`.
    pub path: String,
    pub relation_to: String,
    pub id: Uuid,
    /// The field's `maxDepth`, if it caps population.
    pub max_depth: Option<u8>,
}

/// Reads a `{relationTo, value}` link to any collection; rich text checked the collection.
const ANY: RelationTo = RelationTo::Many(Vec::new());

/// Every relationship ID in a validated document, in schema order.
pub fn relations(fields: &[Field], doc: &Map<String, Value>) -> Vec<RelationRef> {
    let fields: &[Field] = &data_fields(fields);
    fn walk(fields: &[Field], obj: &Map<String, Value>, prefix: &str, out: &mut Vec<RelationRef>) {
        for f in fields {
            let Some(value) = obj.get(&f.name) else {
                continue;
            };
            let path = join(prefix, &f.name);
            let max_depth = match f.kind {
                FieldKind::Relationship { max_depth, .. } => max_depth,
                _ => None,
            };
            // A link at `path` as stored for `relation_to`, if it is one.
            let found = |path: String, relation_to: &RelationTo, link: &Value| {
                let (slug, id) = match (relation_to, link) {
                    (RelationTo::One(slug), Value::String(id)) => (slug.as_str(), id.as_str()),
                    (RelationTo::Many(_), Value::Object(o)) => (
                        o.get("relationTo").and_then(Value::as_str)?,
                        o.get("value").and_then(Value::as_str)?,
                    ),
                    _ => return None,
                };
                Some(RelationRef {
                    path,
                    relation_to: slug.to_owned(),
                    id: Uuid::try_parse(id).ok()?,
                    max_depth,
                })
            };
            match (&f.kind, value) {
                (
                    FieldKind::Relationship {
                        relation_to,
                        has_many: false,
                        ..
                    },
                    id,
                ) => out.extend(found(path, relation_to, id)),
                (
                    FieldKind::Relationship {
                        relation_to,
                        has_many: true,
                        ..
                    },
                    Value::Array(ids),
                ) => {
                    for (i, id) in ids.iter().enumerate() {
                        out.extend(found(format!("{path}.{i}"), relation_to, id));
                    }
                }
                (FieldKind::Group { fields }, Value::Object(inner)) => {
                    walk(fields, inner, &path, out)
                }
                (FieldKind::Array { .. } | FieldKind::Blocks { .. }, Value::Array(rows)) => {
                    for (i, row) in rows.iter().enumerate() {
                        if let Value::Object(inner) = row {
                            walk(
                                row_fields(&f.kind, inner),
                                inner,
                                &format!("{path}.{i}"),
                                out,
                            );
                        }
                    }
                }
                // The k-th link or block node in the text is `<field>.<k>`.
                (FieldKind::RichText { .. }, text) => {
                    for (k, item) in crate::richtext::items(text).into_iter().enumerate() {
                        let here = format!("{path}.{k}");
                        match item {
                            crate::richtext::Item::Link(link) => {
                                out.extend(found(here, &ANY, link))
                            }
                            crate::richtext::Item::Block(attrs) => {
                                if let Some(Value::Object(inner)) = attrs.get("fields") {
                                    walk(node_fields(&f.kind, attrs), inner, &here, out);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(fields, doc, "", &mut out);
    out
}

/// Replaces each relationship ID with `f(path, id)` wherever `f` returns a value. Paths are
/// the ones `relations` reports (`author`, `related.1`, `links.0.author`). Used to populate.
pub fn map_relations(
    fields: &[Field],
    doc: &mut Map<String, Value>,
    f: &mut impl FnMut(&str, &str) -> Option<Value>,
) {
    map_inner(&data_fields(fields), doc, "", f)
}

fn map_inner(
    fields: &[Field],
    doc: &mut Map<String, Value>,
    prefix: &str,
    f: &mut impl FnMut(&str, &str) -> Option<Value>,
) {
    for field in fields {
        let Some(value) = doc.get_mut(&field.name) else {
            continue;
        };
        let path = join(prefix, &field.name);
        match (&field.kind, value) {
            (
                FieldKind::Relationship {
                    has_many: false, ..
                },
                slot,
            ) => replace_link(slot, &mut |id| f(&path, id)),
            (FieldKind::Relationship { has_many: true, .. }, Value::Array(links)) => {
                for (i, slot) in links.iter_mut().enumerate() {
                    let here = format!("{path}.{i}");
                    replace_link(slot, &mut |id| f(&here, id));
                }
            }
            (FieldKind::Group { fields }, Value::Object(inner)) => {
                map_inner(fields, inner, &path, f)
            }
            (kind @ (FieldKind::Array { .. } | FieldKind::Blocks { .. }), Value::Array(rows)) => {
                for (i, row) in rows.iter_mut().enumerate() {
                    if let Value::Object(inner) = row {
                        map_inner(row_fields(kind, inner), inner, &format!("{path}.{i}"), f);
                    }
                }
            }
            (kind @ FieldKind::RichText { .. }, text) => {
                crate::richtext::each_item_mut(text, &mut |k, item| {
                    let here = format!("{path}.{k}");
                    match item {
                        crate::richtext::ItemMut::Link(link) => {
                            replace_link(link, &mut |id| f(&here, id))
                        }
                        crate::richtext::ItemMut::Block(attrs) => {
                            let fields = node_fields(kind, attrs);
                            if let Some(Value::Object(inner)) = attrs.get_mut("fields") {
                                map_inner(fields, inner, &here, f);
                            }
                        }
                    }
                })
            }
            _ => {}
        }
    }
}

/// Polymorphic links keep their `relationTo`; only the `value` inside is replaced.
fn replace_link(slot: &mut Value, f: &mut impl FnMut(&str) -> Option<Value>) {
    match slot {
        Value::Object(o) => {
            if let Some(inner) = o.get_mut("value") {
                replace_id(inner, f);
            }
        }
        other => replace_id(other, f),
    }
}

/// `{"relationTo": "<allowed slug>", "value": "<uuid>"}` and nothing else.
fn is_link(v: &Value, relation_to: &RelationTo) -> bool {
    v.as_object().is_some_and(|o| {
        o.len() == 2
            && o.get("relationTo")
                .and_then(Value::as_str)
                .is_some_and(|s| relation_to.allows(s))
            && o.get("value").and_then(Value::as_str).is_some_and(is_uuid)
    })
}

fn canonical_link(link: &mut Value) {
    match link {
        Value::String(s) => canonical_id(s),
        Value::Object(o) => {
            if let Some(Value::String(s)) = o.get_mut("value") {
                canonical_id(s);
            }
        }
        _ => {}
    }
}

fn replace_id(slot: &mut Value, f: &mut impl FnMut(&str) -> Option<Value>) {
    if let Some(found) = slot.as_str().and_then(f) {
        *slot = found;
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
            json!({"at": "2026-10-12T07:30:00.000Z", "times": [{"at": "2026-10-12T01:15:00.000Z"}]})
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

    #[test]
    fn slugs_are_generated_and_validated() {
        let f = fields(json!([
            {"name": "title", "type": "text"},
            {"name": "slug", "type": "slug", "from": "title"}
        ]));
        assert_eq!(
            ok(&f, json!({"title": "Hello, World! Café"}))["slug"],
            "hello-world-café"
        );
        assert_eq!(
            ok(&f, json!({"title": "x", "slug": "my-own"}))["slug"],
            "my-own"
        );
        for bad in ["Has-Caps", "two--dashes", "-lead", "trail-", "sp ace", ""] {
            assert_eq!(
                errs(&f, json!({"slug": bad})),
                vec![e(
                    "slug",
                    "expected lowercase letters, digits and single hyphens, like my-first-post"
                )],
                "{bad}"
            );
        }
    }

    #[test]
    fn relationships_hold_document_ids() {
        let f = fields(json!([
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "related", "type": "relationship", "relationTo": "posts", "hasMany": true}
        ]));
        let id = "0199D5C4-0000-7000-8000-000000000001";
        let doc = ok(&f, json!({"author": id, "related": [id]}));
        assert_eq!(
            doc["author"],
            id.to_lowercase(),
            "IDs are stored in canonical form"
        );
        assert_eq!(
            errs(&f, json!({"author": "nope"})),
            vec![e("author", "expected a document ID")]
        );
        assert_eq!(
            errs(&f, json!({"related": id})),
            vec![e("related", "expected a list of document IDs")]
        );
    }

    #[test]
    fn relations_are_found_everywhere() {
        let f = fields(json!([
            {"name": "author", "type": "relationship", "relationTo": "authors"},
            {"name": "related", "type": "relationship", "relationTo": "posts", "hasMany": true},
            {"name": "links", "type": "array", "fields": [
                {"name": "author", "type": "relationship", "relationTo": "authors"}
            ]}
        ]));
        let a = "0199d5c4-0000-7000-8000-00000000000a";
        let b = "0199d5c4-0000-7000-8000-00000000000b";
        let doc = ok(
            &f,
            json!({"author": a, "related": [a, b], "links": [{}, {"author": b}]}),
        );
        let found: Vec<(String, String, String)> = relations(&f, &doc)
            .into_iter()
            .map(|r| (r.path, r.relation_to, r.id.to_string()))
            .collect();
        let expected: Vec<(String, String, String)> = [
            ("author", "authors", a),
            ("related.0", "posts", a),
            ("related.1", "posts", b),
            ("links.1.author", "authors", b),
        ]
        .into_iter()
        .map(|(p, r, i)| (p.into(), r.into(), i.into()))
        .collect();
        assert_eq!(found, expected);

        let mut doc = doc;
        map_relations(&f, &mut doc, &mut |_, id| {
            (id == b).then(|| json!({"id": b, "name": "Bee"}))
        });
        assert_eq!(doc["author"], a, "unmatched IDs stay as they are");
        assert_eq!(doc["related"][1]["name"], "Bee");
        assert_eq!(doc["links"][1]["author"]["name"], "Bee");
    }

    #[test]
    fn normalize_date_time_pads_to_milliseconds() {
        assert_eq!(
            normalize_date_time("2026-10-12T09:30:00+02:00").as_deref(),
            Some("2026-10-12T07:30:00.000Z")
        );
        assert_eq!(
            normalize_date_time("2026-10-12T07:30:00.123456Z").as_deref(),
            Some("2026-10-12T07:30:00.123Z")
        );
        assert_eq!(normalize_date_time("yesterday"), None);
    }

    #[test]
    fn date_times_outside_years_0_to_9999_are_rejected_not_panics() {
        assert_eq!(normalize_date_time("9999-12-31T23:00:00-02:00"), None);
        assert_eq!(normalize_date_time("0000-01-01T00:00:00+01:00"), None);
        let f = fields(json!([{"name": "at", "type": "date"}]));
        assert_eq!(
            errs(&f, json!({"at": "9999-12-31T23:00:00-02:00"})),
            vec![e("at", "expected a date-time like 2026-10-12T09:30:00Z")]
        );
    }
    #[test]
    fn labelled_options_validate_by_value() {
        let f = fields(json!([
            {"name": "status", "type": "select", "options": [{"label": "Ready to go", "value": "ready"}]},
            {"name": "size", "type": "radio", "options": ["s", "l"]},
            {"name": "snippet", "type": "code"},
            {"name": "intro", "type": "textarea", "minLength": 3}
        ]));
        ok(
            &f,
            json!({"status": "ready", "size": "l", "snippet": "fn main() {}", "intro": "abc"}),
        );
        assert_eq!(
            errs(&f, json!({"status": "Ready to go"})),
            vec![e("status", "expected one of: ready")]
        );
        assert_eq!(
            errs(&f, json!({"size": "m"})),
            vec![e("size", "expected one of: s, l")]
        );
        assert_eq!(
            errs(&f, json!({"snippet": 1})),
            vec![e("snippet", "expected text")]
        );
        assert_eq!(
            errs(&f, json!({"intro": "ab"})),
            vec![e("intro", "must be at least 3 characters")]
        );
    }
    #[test]
    fn has_many_text_and_numbers_validate_each_value() {
        let f = fields(json!([
            {"name": "tags", "type": "text", "hasMany": true, "maxLength": 5, "maxRows": 2},
            {"name": "scores", "type": "number", "hasMany": true, "integer": true, "minRows": 1}
        ]));
        ok(&f, json!({"tags": ["a", "b"], "scores": [1, 2, 3]}));
        assert_eq!(
            errs(&f, json!({"tags": "a"})),
            vec![e("tags", "expected a list")]
        );
        assert_eq!(
            errs(&f, json!({"tags": ["ok", "toolong"]})),
            vec![e("tags.1", "must be at most 5 characters")]
        );
        assert_eq!(
            errs(&f, json!({"tags": ["a", "b", "c"]})),
            vec![e("tags", "must have at most 2 rows")]
        );
        assert_eq!(
            errs(&f, json!({"scores": []})),
            vec![e("scores", "must have at least 1 row")]
        );
        assert_eq!(
            errs(&f, json!({"scores": [1.5]})),
            vec![e("scores.0", "expected a whole number")]
        );
    }
    #[test]
    fn layout_fields_validate_as_flat_data() {
        let f = fields(json!([
            {"type": "row", "fields": [{"name": "title", "type": "text", "required": true}]},
            {"type": "tabs", "tabs": [
                {"label": "A", "fields": [{"name": "body", "type": "textarea"}]},
                {"name": "meta", "label": "B", "fields": [{"name": "author", "type": "relationship", "relationTo": "authors"}]}
            ]}
        ]));
        let id = "0199d5c4-0000-7000-8000-00000000000a";
        let doc = ok(
            &f,
            json!({"title": "T", "body": "B", "meta": {"author": id}}),
        );
        assert_eq!(relations(&f, &doc)[0].path, "meta.author");
        assert_eq!(
            errs(&f, json!({"body": "B"})),
            vec![e("title", "is required")]
        );
        assert_eq!(
            errs(&f, json!({"title": "T", "row": {}})),
            vec![e("row", "unknown field")]
        );
    }
    #[test]
    fn polymorphic_links_validate_and_are_found() {
        let f = fields(json!([
            {"name": "link", "type": "relationship", "relationTo": ["posts", "pages"]},
            {"name": "more", "type": "relationship", "relationTo": ["posts", "pages"], "hasMany": true}
        ]));
        let a = "0199D5C4-0000-7000-8000-00000000000A";
        let doc = ok(
            &f,
            json!({
                "link": {"relationTo": "pages", "value": a},
                "more": [{"relationTo": "posts", "value": a}]
            }),
        );
        assert_eq!(
            doc["link"]["value"],
            a.to_lowercase(),
            "IDs inside links are canonical"
        );
        let found: Vec<(String, String)> = relations(&f, &doc)
            .into_iter()
            .map(|r| (r.path, r.relation_to))
            .collect();
        assert_eq!(
            found,
            [
                ("link".to_owned(), "pages".to_owned()),
                ("more.0".to_owned(), "posts".to_owned())
            ]
        );

        let one = "expected a link like {\"relationTo\": \"<collection>\", \"value\": \"<document ID>\"} to one of: posts, pages";
        for bad in [
            json!(a),
            json!({"relationTo": "authors", "value": a}),
            json!({"relationTo": "pages", "value": "x"}),
            json!({"relationTo": "pages", "value": a, "extra": 1}),
        ] {
            assert_eq!(
                errs(&f, json!({"link": bad})),
                vec![e("link", one)],
                "{bad}"
            );
        }

        let mut doc = doc;
        map_relations(&f, &mut doc, &mut |_, id| {
            Some(json!({"id": id, "title": "T"}))
        });
        assert_eq!(doc["link"]["relationTo"], "pages");
        assert_eq!(doc["link"]["value"]["title"], "T");
        assert_eq!(doc["more"][0]["value"]["title"], "T");
    }
    #[test]
    fn points_are_longitude_latitude_pairs() {
        let f = fields(json!([{"name": "where", "type": "point"}]));
        ok(&f, json!({"where": [-0.1276, 51.5072]}));
        let message = "expected [longitude, latitude], like [-0.1276, 51.5072]";
        for bad in [
            json!([0]),
            json!([181, 0]),
            json!([0, 91]),
            json!(["0", "0"]),
            json!({"lng": 0, "lat": 0}),
        ] {
            assert_eq!(
                errs(&f, json!({"where": bad})),
                vec![e("where", message)],
                "{bad}"
            );
        }
    }

    fn update(fields: &[Field], existing: Value, patch: Value) -> Value {
        let Value::Object(existing) = existing else {
            panic!("existing must be an object")
        };
        Value::Object(validate_document(fields, &patch, Some(&existing)).expect("should be valid"))
    }

    fn page_fields() -> Vec<Field> {
        fields(json!([
            {"name": "seo", "type": "group", "fields": [
                {"name": "title", "type": "text"},
                {"name": "description", "type": "text"}
            ]},
            {"name": "tags", "type": "text", "hasMany": true},
            {"name": "links", "type": "array", "fields": [
                {"name": "label", "type": "text"},
                {"name": "url", "type": "url"}
            ]},
            {"name": "meta", "type": "json"}
        ]))
    }

    #[test]
    fn update_merges_groups_and_replaces_lists() {
        let f = page_fields();
        assert_eq!(
            update(
                &f,
                json!({"seo": {"title": "a", "description": "d"}, "tags": ["a", "b"]}),
                json!({"seo": {"title": "x"}, "tags": ["c"]})
            ),
            json!({"seo": {"title": "x", "description": "d"}, "tags": ["c"]})
        );
    }

    #[test]
    fn null_clears_a_field_at_any_depth() {
        let f = page_fields();
        assert_eq!(
            update(
                &f,
                json!({"seo": {"title": "a", "description": "d"}}),
                json!({"seo": {"description": null}})
            ),
            json!({"seo": {"title": "a"}})
        );
        assert_eq!(
            Value::Object(ok(
                &f,
                json!({"seo": {"title": "t", "description": null},
                       "links": [{"label": null, "url": "https://example.com"}]})
            )),
            json!({"seo": {"title": "t"}, "links": [{"url": "https://example.com"}]})
        );
    }

    #[test]
    fn nulls_inside_json_fields_are_kept() {
        let f = page_fields();
        assert_eq!(
            Value::Object(ok(&f, json!({"meta": {"a": null}}))),
            json!({"meta": {"a": null}})
        );
        assert_eq!(
            update(
                &f,
                json!({"meta": {"a": 1, "b": 2}}),
                json!({"meta": {"a": null}})
            ),
            json!({"meta": {"a": null}}),
            "json values are replaced, not merged"
        );
    }

    #[test]
    fn defaults_fill_groups_and_rows() {
        let f = fields(json!([
            {"name": "seo", "type": "group", "fields": [
                {"name": "index", "type": "boolean", "default": true},
                {"name": "title", "type": "text"}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "label", "type": "text"},
                {"name": "external", "type": "boolean", "default": false}
            ]}
        ]));
        assert_eq!(
            Value::Object(ok(&f, json!({"links": [{"label": "a"}]}))),
            json!({"seo": {"index": true}, "links": [{"label": "a", "external": false}]})
        );
        assert_eq!(
            update(
                &f,
                json!({"seo": {"title": "t"}, "links": []}),
                json!({"links": [{"label": "b"}]})
            ),
            json!({"seo": {"title": "t"}, "links": [{"label": "b", "external": false}]}),
            "rows sent in an update get their defaults; groups keep what's stored"
        );
    }

    #[test]
    fn required_lists_reject_empty() {
        let f = fields(json!([
            {"name": "tags", "type": "text", "hasMany": true, "required": true},
            {"name": "related", "type": "relationship", "relationTo": "posts", "hasMany": true, "required": true},
            {"name": "colours", "type": "select", "options": ["red"], "multiple": true, "required": true},
            {"name": "rows", "type": "array", "required": true, "fields": [{"name": "x", "type": "text"}]}
        ]));
        assert_eq!(
            errs(
                &f,
                json!({"tags": [], "related": [], "colours": [], "rows": []})
            ),
            vec![
                e("tags", "is required"),
                e("related", "is required"),
                e("colours", "is required"),
                e("rows", "is required"),
            ]
        );
    }

    #[test]
    fn date_times_keep_milliseconds_consistently() {
        // Stored values and filter values go through the same function, so they always agree.
        let stored = normalize_date_time("2026-10-12T09:30:00.123456Z").unwrap();
        assert_eq!(stored, "2026-10-12T09:30:00.123Z");
        assert_eq!(
            normalize_date_time("2026-10-12T11:30:00.123999+02:00").as_deref(),
            Some(stored.as_str())
        );
    }

    fn block_fields() -> Vec<Field> {
        fields(json!([
            {"name": "layout", "type": "blocks", "minRows": 1, "maxRows": 3, "blocks": [
                {"slug": "hero", "fields": [
                    {"name": "heading", "type": "text", "required": true},
                    {"type": "row", "fields": [{"name": "dark", "type": "boolean", "default": false}]}
                ]},
                {"slug": "quote", "fields": [
                    {"name": "text", "type": "textarea", "default": "…"},
                    {"name": "author", "type": "relationship", "relationTo": "authors"}
                ]}
            ]}
        ]))
    }

    #[test]
    fn block_rows_validate_against_their_type() {
        let f = block_fields();
        let id = "0199D5C4-0000-7000-8000-0000000000AA";
        let doc = ok(
            &f,
            json!({"layout": [
                {"blockType": "hero", "heading": "Hi", "id": id},
                {"blockType": "quote", "author": null}
            ]}),
        );
        let rows = doc["layout"].as_array().unwrap();
        assert_eq!(
            rows[0],
            json!({"id": id.to_lowercase(), "blockType": "hero", "heading": "Hi", "dark": false}),
            "an existing id is kept; defaults follow the block type"
        );
        let new_id = rows[1]["id"].as_str().expect("new rows get an id");
        assert!(Uuid::try_parse(new_id).is_ok());
        assert_eq!(rows[1]["text"], "…");
        assert!(rows[1].get("dark").is_none() && rows[1].get("author").is_none());

        assert_eq!(
            errs(
                &f,
                json!({"layout": [
                    {"blockType": "hero"},
                    {"blockType": "quote", "heading": "x"},
                    {"blockType": "nope"},
                    {"heading": "x"},
                    "row"
                ]})
            ),
            vec![
                e("layout", "must have at most 3 rows"),
                e("layout.0.heading", "is required"),
                e("layout.1.heading", "unknown field"),
                e("layout.2.blockType", "expected one of: hero, quote"),
                e("layout.3.blockType", "is required"),
                e("layout.4", "expected an object"),
            ]
        );
        assert_eq!(
            errs(&f, json!({"layout": []})),
            vec![e("layout", "must have at least 1 row")]
        );
        assert_eq!(
            errs(&f, json!({"layout": {"blockType": "hero"}})),
            vec![e("layout", "expected a list")]
        );
        assert_eq!(
            errs(
                &f,
                json!({"layout": [{"blockType": "hero", "heading": "x", "id": "nope"}]})
            ),
            vec![e("layout.0.id", "expected a row ID")]
        );
        let lower = "0199d5c4-0000-7000-8000-0000000000bb";
        for twin in [lower, &lower.to_uppercase()] {
            assert_eq!(
                errs(
                    &f,
                    json!({"layout": [
                        {"blockType": "hero", "heading": "a", "id": lower},
                        {"blockType": "quote", "id": twin}
                    ]})
                ),
                vec![e("layout.1.id", "this row ID is used twice")],
                "{twin}"
            );
        }

        // An update replaces the list whole: rows sent with their id keep it.
        let existing = Value::Object(doc.clone());
        let updated = update(
            &f,
            existing,
            json!({"layout": [rows[1].clone(), {"blockType": "hero", "heading": "New"}]}),
        );
        assert_eq!(updated["layout"][0]["id"], new_id);
        assert_eq!(updated["layout"][1]["dark"], false);
        assert_ne!(updated["layout"][1]["id"], new_id);
        assert!(updated["layout"][1]["id"].is_string());

        let required = fields(
            json!([{"name": "layout", "type": "blocks", "required": true,
            "blocks": [{"slug": "hero", "fields": []}]}]),
        );
        assert_eq!(
            errs(&required, json!({"layout": []})),
            vec![e("layout", "is required")]
        );
    }

    #[test]
    fn relationships_inside_blocks_are_relations() {
        let f = block_fields();
        let b = "0199d5c4-0000-7000-8000-00000000000b";
        let mut doc = ok(
            &f,
            json!({"layout": [
                {"blockType": "hero", "heading": "Hi"},
                {"blockType": "quote", "author": b}
            ]}),
        );
        let found: Vec<(String, String, String)> = relations(&f, &doc)
            .into_iter()
            .map(|r| (r.path, r.relation_to, r.id.to_string()))
            .collect();
        assert_eq!(
            found,
            [("layout.1.author".into(), "authors".into(), b.into())]
        );
        map_relations(&f, &mut doc, &mut |path, id| {
            assert_eq!(path, "layout.1.author");
            Some(json!({"id": id, "name": "Bee"}))
        });
        assert_eq!(doc["layout"][1]["author"]["name"], "Bee");
    }

    #[test]
    fn nested_blocks_follow_their_own_types() {
        let f = fields(json!([{"name": "layout", "type": "blocks", "blocks": [
            {"slug": "section", "fields": [{"name": "items", "type": "blocks", "blocks": [
                {"slug": "card", "fields": [
                    {"name": "title", "type": "text", "required": true},
                    {"name": "author", "type": "relationship", "relationTo": "authors"},
                    {"name": "tone", "type": "text", "default": "plain"}
                ]},
                {"slug": "rule", "fields": []}
            ]}]}
        ]}]));
        let b = "0199d5c4-0000-7000-8000-00000000000b";
        let mut doc = ok(
            &f,
            json!({"layout": [{"blockType": "section", "items": [
                {"blockType": "rule"},
                {"blockType": "card", "title": "T", "author": b}
            ]}]}),
        );
        let card = doc["layout"][0]["items"][1].clone();
        assert_eq!(
            card["tone"], "plain",
            "inner defaults follow the inner type"
        );
        assert!(card["id"].is_string() && doc["layout"][0]["id"].is_string());
        assert!(doc["layout"][0]["items"][0].get("tone").is_none());

        assert_eq!(
            errs(
                &f,
                json!({"layout": [{"blockType": "section", "items": [
                    {"blockType": "card"}, {"blockType": "rule", "title": "x"}, {"blockType": "section"}
                ]}]})
            ),
            vec![
                e("layout.0.items.0.title", "is required"),
                e("layout.0.items.1.title", "unknown field"),
                e("layout.0.items.2.blockType", "expected one of: card, rule"),
            ]
        );

        let found: Vec<String> = relations(&f, &doc).into_iter().map(|r| r.path).collect();
        assert_eq!(found, ["layout.0.items.1.author"]);
        map_relations(&f, &mut doc, &mut |_, id| {
            Some(json!({"id": id, "name": "Bee"}))
        });
        assert_eq!(doc["layout"][0]["items"][1]["author"]["name"], "Bee");

        let params = crate::query::parse_query("select[layout][items][title]=true").unwrap();
        let select = crate::select::parse_select(&f, &params["select"], "select").unwrap();
        let mut shaped = doc.clone();
        crate::select::project(&select, &mut shaped);
        let rows = &shaped["layout"][0];
        assert_eq!(rows["blockType"], "section");
        assert_eq!(
            rows["items"][1],
            json!({"id": card["id"], "blockType": "card", "title": "T"})
        );
    }

    #[test]
    fn groups_new_to_a_document_get_their_defaults() {
        let f = fields(json!([
            {"name": "seo", "type": "group", "fields": [
                {"name": "index", "type": "boolean", "default": true},
                {"name": "title", "type": "text"}
            ]},
            {"name": "card", "type": "group", "default": {"a": "x"}, "fields": [
                {"name": "a", "type": "text"},
                {"name": "b", "type": "text", "default": "bd"}
            ]}
        ]));
        assert_eq!(
            update(&f, json!({}), json!({"seo": {"title": "new"}})),
            json!({"seo": {"title": "new", "index": true}}),
            "a group the update creates is as new as a row"
        );
        assert_eq!(
            Value::Object(ok(&f, json!({}))),
            json!({"seo": {"index": true}, "card": {"a": "x", "b": "bd"}}),
            "a group's own default still gets its children's defaults"
        );
    }
}
