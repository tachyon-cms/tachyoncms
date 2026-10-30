//! `select`: which fields a read returns.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::query::FilterError;
use crate::schema::{BLOCK_ROW_KEYS, BlockSource, Field, FieldKind, data_fields};

/// Keys every document carries besides its fields. `id` always comes back.
const BUILT_INS: &[&str] = &["id", "createdAt", "updatedAt", "status"];

#[derive(Debug, Clone, PartialEq)]
pub enum Select {
    /// Only these, plus `id`.
    Include(Tree),
    /// Everything but these (never `id`).
    Exclude(Tree),
}

/// Field names to their selected children; `None` takes or drops the whole value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tree(pub BTreeMap<String, Option<Tree>>);

/// Reads `select` as `parse_query` nests it, such as `{"title": "true", "seo": {"title": "true"}}`.
/// `param` names the parameter in errors (`select`, or `populate[authors]`).
pub fn parse_select(fields: &[Field], input: &Value, param: &str) -> Result<Select, FilterError> {
    let err = |param: &str, message: &str| FilterError {
        param: param.to_owned(),
        message: message.to_owned(),
    };
    let Value::Object(input) = input else {
        return Err(err(
            param,
            "expected select[field]=true or select[field]=false",
        ));
    };
    let fields: &[Field] = &data_fields(fields);
    let mut modes = Vec::new();
    let tree = parse_tree(&[fields], input, param, true, false, &mut modes)?;
    match (modes.contains(&true), modes.contains(&false)) {
        (true, true) => Err(err(param, "use either true or false throughout, not both")),
        (false, true) => Ok(Select::Exclude(tree)),
        _ => Ok(Select::Include(tree)),
    }
}

/// `sets` are the field lists a name can come from: one, or every block type's inside a
/// `blocks` field (`block_rows`), since a select there doesn't name the block type.
fn parse_tree(
    sets: &[&[Field]],
    input: &Map<String, Value>,
    param: &str,
    top: bool,
    block_rows: bool,
    modes: &mut Vec<bool>,
) -> Result<Tree, FilterError> {
    let mut tree = Tree::default();
    for (name, value) in input {
        let here = format!("{param}[{name}]");
        let err = |message: String| FilterError {
            param: here.clone(),
            message,
        };
        let matches: Vec<&Field> = sets
            .iter()
            .flat_map(|s| s.iter())
            .filter(|f| &f.name == name)
            .collect();
        let field = matches.first();
        let built_in = top && BUILT_INS.contains(&name.as_str())
            || block_rows && BLOCK_ROW_KEYS.contains(&name.as_str());
        if field.is_none() && !built_in {
            return Err(err("unknown field".into()));
        }
        let child = match value {
            // Text from a query string, or a boolean from a schema's `defaultPopulate`.
            Value::Bool(b) => {
                modes.push(*b);
                None
            }
            Value::String(s) => {
                modes.push(match s.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(err("expected true or false".into())),
                });
                None
            }
            Value::Object(inner) => {
                let inside: Vec<&[Field]> = matches
                    .iter()
                    .flat_map(|f| match &f.kind {
                        FieldKind::Group { fields } | FieldKind::Array { fields, .. } => {
                            vec![fields.as_slice()]
                        }
                        FieldKind::Blocks { blocks, .. } => blocks
                            .iter()
                            .filter_map(BlockSource::def)
                            .map(|b| b.fields.as_slice())
                            .collect(),
                        _ => vec![],
                    })
                    .collect();
                match (field, inside.is_empty()) {
                    (Some(f), true) => {
                        return Err(err(format!(
                            "{} fields have nothing inside to select",
                            f.kind.name()
                        )));
                    }
                    (None, _) => {
                        return Err(err(format!("{name} has nothing inside to select")));
                    }
                    (Some(_), false) => {
                        let blocks = matches
                            .iter()
                            .any(|f| matches!(f.kind, FieldKind::Blocks { .. }));
                        Some(parse_tree(&inside, inner, &here, false, blocks, modes)?)
                    }
                }
            }
            _ => return Err(err("expected true or false".into())),
        };
        tree.0.insert(name.clone(), child);
    }
    // A block row always says what it is. An empty subtree keeps a value untouched under
    // both include and exclude, whichever the select turns out to be.
    if block_rows {
        for key in BLOCK_ROW_KEYS {
            tree.0.insert((*key).to_owned(), Some(Tree::default()));
        }
    }
    Ok(tree)
}

impl Select {
    /// Whether the value at a dotted path survives `project`.
    pub fn keeps(&self, path: &str) -> bool {
        let mut parts = path.split('.');
        match self {
            Select::Include(tree) => {
                let mut tree = tree;
                for part in parts.by_ref() {
                    match tree.0.get(part) {
                        None => return false,
                        Some(None) => return true,
                        Some(Some(child)) => tree = child,
                    }
                }
                true
            }
            Select::Exclude(tree) => {
                let mut tree = tree;
                for part in parts.by_ref() {
                    match tree.0.get(part) {
                        None => return true,
                        Some(None) => return false,
                        Some(Some(child)) => tree = child,
                    }
                }
                true
            }
        }
    }
}

/// Shapes `doc` in place.
pub fn project(select: &Select, doc: &mut Map<String, Value>) {
    match select {
        Select::Include(tree) => {
            let id = doc.remove("id");
            include(tree, doc);
            if let Some(id) = id {
                doc.insert("id".into(), id);
            }
        }
        Select::Exclude(tree) => {
            let id = doc.remove("id");
            exclude(tree, doc);
            if let Some(id) = id {
                doc.insert("id".into(), id);
            }
        }
    }
}

/// Applies `f` to an object, or to every object row of a list.
fn each_object(value: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    match value {
        Value::Object(obj) => f(obj),
        Value::Array(rows) => rows.iter_mut().filter_map(Value::as_object_mut).for_each(f),
        _ => {}
    }
}

fn include(tree: &Tree, obj: &mut Map<String, Value>) {
    obj.retain(|key, _| tree.0.contains_key(key));
    for (key, child) in &tree.0 {
        if let (Some(child), Some(value)) = (child, obj.get_mut(key)) {
            each_object(value, &mut |inner| include(child, inner));
        }
    }
}

fn exclude(tree: &Tree, obj: &mut Map<String, Value>) {
    for (key, child) in &tree.0 {
        match child {
            None => {
                obj.remove(key);
            }
            Some(child) => {
                if let Some(value) = obj.get_mut(key) {
                    each_object(value, &mut |inner| exclude(child, inner));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;
    use serde_json::json;

    fn fields() -> Vec<Field> {
        serde_json::from_value(json!([
            {"name": "title", "type": "text"},
            {"name": "body", "type": "textarea"},
            {"name": "seo", "type": "group", "fields": [
                {"name": "title", "type": "text"}, {"name": "description", "type": "text"}
            ]},
            {"name": "links", "type": "array", "fields": [
                {"name": "label", "type": "text"}, {"name": "url", "type": "url"}
            ]}
        ]))
        .unwrap()
    }

    fn select(qs: &str) -> Result<Select, String> {
        let params = parse_query(qs).unwrap();
        parse_select(&fields(), &params["select"], "select").map_err(|e| e.to_string())
    }

    fn doc() -> Map<String, Value> {
        json!({
            "id": "1", "status": "published", "createdAt": "c", "updatedAt": "u",
            "title": "T", "body": "B",
            "seo": {"title": "ST", "description": "SD"},
            "links": [{"label": "a", "url": "https://a.test"}, {"label": "b", "url": "https://b.test"}]
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn shaped(qs: &str) -> Value {
        let mut d = doc();
        project(&select(qs).unwrap(), &mut d);
        Value::Object(d)
    }

    #[test]
    fn select_includes_only_what_is_named_plus_id() {
        assert_eq!(
            shaped("select[title]=true&select[createdAt]=true"),
            json!({"id": "1", "title": "T", "createdAt": "c"})
        );
    }

    #[test]
    fn select_excludes_what_is_named() {
        assert_eq!(
            shaped("select[body]=false&select[seo]=false&select[links]=false&select[status]=false"),
            json!({"id": "1", "createdAt": "c", "updatedAt": "u", "title": "T"})
        );
    }

    #[test]
    fn select_reaches_into_groups_and_every_array_row() {
        assert_eq!(
            shaped("select[seo][title]=true&select[links][url]=true"),
            json!({"id": "1", "seo": {"title": "ST"},
                   "links": [{"url": "https://a.test"}, {"url": "https://b.test"}]})
        );
        assert_eq!(
            shaped("select[seo][title]=false&select[links][label]=false&select[body]=false"),
            json!({"id": "1", "status": "published", "createdAt": "c", "updatedAt": "u", "title": "T",
                   "seo": {"description": "SD"},
                   "links": [{"url": "https://a.test"}, {"url": "https://b.test"}]})
        );
    }

    #[test]
    fn select_reaches_into_blocks() {
        let fields: Vec<Field> = serde_json::from_value(json!([
            {"name": "title", "type": "text"},
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "hero", "fields": [{"name": "heading", "type": "text"}, {"name": "sub", "type": "text"}]},
                {"slug": "cards", "fields": [
                    {"name": "heading", "type": "text"},
                    {"name": "items", "type": "array", "fields": [{"name": "label", "type": "text"}, {"name": "n", "type": "number"}]}
                ]}
            ]}
        ]))
        .unwrap();
        let doc = json!({"id": "1", "title": "T", "layout": [
            {"id": "r1", "blockType": "hero", "heading": "H", "sub": "S"},
            {"id": "r2", "blockType": "cards", "heading": "C", "items": [{"label": "a", "n": 1}]}
        ]});
        let shaped = |qs: &str| {
            let params = parse_query(qs).unwrap();
            let select = parse_select(&fields, &params["select"], "select")?;
            let mut d = doc.as_object().unwrap().clone();
            project(&select, &mut d);
            Ok::<_, FilterError>(Value::Object(d))
        };
        assert_eq!(
            shaped("select[layout][heading]=true").unwrap(),
            json!({"id": "1", "layout": [
                {"id": "r1", "blockType": "hero", "heading": "H"},
                {"id": "r2", "blockType": "cards", "heading": "C"}
            ]})
        );
        assert_eq!(
            shaped(
                "select[layout][heading]=false&select[layout][items][n]=false&select[title]=false"
            )
            .unwrap(),
            json!({"id": "1", "layout": [
                {"id": "r1", "blockType": "hero", "sub": "S"},
                {"id": "r2", "blockType": "cards", "items": [{"label": "a"}]}
            ]})
        );
        assert_eq!(
            shaped("select[layout][blockType]=false").unwrap()["layout"][0]["blockType"],
            "hero",
            "a row always says what it is"
        );
        assert_eq!(
            shaped("select[layout][nope]=true").unwrap_err().to_string(),
            "select[layout][nope]: unknown field"
        );
    }

    #[test]
    fn select_keeps_id_even_when_excluded() {
        assert_eq!(shaped("select[id]=false&select[body]=false")["id"], "1");
    }

    #[test]
    fn select_refuses_mixed_unknown_and_bad_values() {
        for (qs, message) in [
            ("select[nope]=true", "select[nope]: unknown field"),
            ("select[seo][nope]=true", "select[seo][nope]: unknown field"),
            (
                "select[title][x]=true",
                "select[title]: text fields have nothing inside to select",
            ),
            (
                "select[title]=true&select[body]=false",
                "select: use either true or false throughout, not both",
            ),
            (
                "select[title]=maybe",
                "select[title]: expected true or false",
            ),
            (
                "select=title",
                "select: expected select[field]=true or select[field]=false",
            ),
            (
                "select[createdAt][x]=true",
                "select[createdAt]: createdAt has nothing inside to select",
            ),
        ] {
            assert_eq!(select(qs).unwrap_err(), message, "{qs}");
        }
    }
}
