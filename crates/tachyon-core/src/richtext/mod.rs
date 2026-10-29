//! Rich text: a tree of nodes and marks, stored as the JSON the editor works in.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::schema::{BlockSource, FieldKind};
use crate::validate::{FieldError, slugify};

mod html;
mod markdown_in;
mod markdown_out;
mod text;

pub use html::to_html;
pub use markdown_in::{from_markdown, slug_refs_mut};
pub use markdown_out::to_markdown;
pub use text::to_text;

/// What the renderers need to know about the documents rich text links to. Each answer
/// takes a link's collection and its `value`: the ID, or the document populated in its
/// place. `None` means the link shows as its text only: the caller can't read the target,
/// or it has no answer to give.
pub struct RenderCtx<'a> {
    /// Where an internal link goes, from its collection's `url`.
    pub link_url: &'a dyn Fn(&str, &Value) -> Option<String>,
    /// The text a relationship node shows in HTML.
    pub title: &'a dyn Fn(&str, &Value) -> Option<String>,
    /// The slug Markdown names a linked document by (`doc:posts/<slug>`).
    pub slug: &'a dyn Fn(&str, &Value) -> Option<String>,
}

/// What a rich text field lets editors use. Paragraphs and plain text are always allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Feature {
    Bold,
    Italic,
    Underline,
    Strike,
    Subscript,
    Superscript,
    Code,
    Link,
    Heading,
    BulletList,
    OrderedList,
    TaskList,
    Blockquote,
    HorizontalRule,
    HardBreak,
    Table,
    CodeBlock,
    Relationship,
    Block,
}

/// A reference rich text holds to another document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRef {
    Doc { collection: String, id: Uuid },
}

/// How deep nodes may nest: the document's own children are 1 deep.
pub const MAX_DEPTH: usize = 32;
/// How many nodes one value may hold, the document itself not counted.
pub const MAX_NODES: usize = 50_000;

/// How many columns or rows one table cell may span.
pub const MAX_SPAN: u64 = 1_000;

const DOC_SHAPE: &str = "expected a rich text document like {\"type\": \"doc\", \"content\": []}";
const SCHEMES: &str = "links must start with http://, https://, mailto:, tel:, / or #";

/// What a node may hold.
#[derive(Clone, Copy, PartialEq)]
enum Holds {
    Nothing,
    /// Text and hard breaks.
    Inline,
    /// Text without marks.
    PlainText,
    /// Block-level nodes, at least one.
    Blocks,
    /// These nodes, at least one.
    Only(&'static [&'static str]),
}

/// Nodes that stand on their own line: what a document, a list item or a cell holds.
const BLOCK_NODES: &[&str] = &[
    "paragraph",
    "heading",
    "bulletList",
    "orderedList",
    "taskList",
    "blockquote",
    "horizontalRule",
    "codeBlock",
    "table",
    "relationship",
    "block",
];

impl Holds {
    fn fits(self, child: &str) -> bool {
        match self {
            Self::Nothing => false,
            Self::Inline => matches!(child, "text" | "hardBreak"),
            Self::PlainText => child == "text",
            Self::Blocks => BLOCK_NODES.contains(&child),
            Self::Only(list) => list.contains(&child),
        }
    }
}

/// A node type: the feature it needs, what it holds and the attributes it takes.
fn node_spec(t: &str) -> Option<(Option<Feature>, Holds, &'static [&'static str])> {
    use Feature as F;
    use Holds::*;
    Some(match t {
        "paragraph" => (None, Inline, &[]),
        "text" => (None, Nothing, &[]),
        "heading" => (Some(F::Heading), Inline, &["level"]),
        "bulletList" => (Some(F::BulletList), Only(&["listItem"]), &[]),
        "orderedList" => (Some(F::OrderedList), Only(&["listItem"]), &["start"]),
        "listItem" => (None, Blocks, &[]),
        "taskList" => (Some(F::TaskList), Only(&["taskItem"]), &[]),
        "taskItem" => (None, Blocks, &["checked"]),
        "blockquote" => (Some(F::Blockquote), Blocks, &[]),
        "horizontalRule" => (Some(F::HorizontalRule), Nothing, &[]),
        "hardBreak" => (Some(F::HardBreak), Nothing, &[]),
        "codeBlock" => (Some(F::CodeBlock), PlainText, &["language"]),
        "table" => (Some(F::Table), Only(&["tableRow"]), &[]),
        "tableRow" => (None, Only(&["tableCell", "tableHeader"]), &[]),
        "tableCell" | "tableHeader" => (None, Blocks, &["colspan", "rowspan", "colwidth"]),
        "relationship" => (Some(F::Relationship), Nothing, &["relationTo", "value"]),
        "block" => (Some(F::Block), Nothing, &["blockType", "id", "fields"]),
        _ => return None,
    })
}

fn mark_feature(t: &str) -> Option<Feature> {
    use Feature as F;
    Some(match t {
        "bold" => F::Bold,
        "italic" => F::Italic,
        "underline" => F::Underline,
        "strike" => F::Strike,
        "subscript" => F::Subscript,
        "superscript" => F::Superscript,
        "code" => F::Code,
        "link" => F::Link,
        _ => return None,
    })
}

/// An `href` a browser can only follow to a web page, an email, a phone number or a place
/// on this site. Schemes match in any case, as browsers read them (W1-R20). Anything with
/// a control character, whitespace or a backslash is refused outright: browsers strip or
/// rewrite those, which is how `java\tscript:` and `/\host` get through looser checks.
pub(crate) fn safe_href(href: &str) -> bool {
    if href
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return false;
    }
    ["http://", "https://", "mailto:", "tel:", "#"]
        .iter()
        .any(|p| {
            href.get(..p.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(p))
        })
        || (href.starts_with('/') && !href.starts_with("//"))
}

/// Checks `value`, a `richText` field's value at `path`, against `kind`: its node and mark
/// set, its features, where each node may go, the limits, and the fields of block nodes.
/// Paths name the offending node, such as `body.content.3.marks.0.attrs.href`.
pub fn validate(value: &Value, kind: &FieldKind, path: &str) -> Result<(), Vec<FieldError>> {
    validate_within(value, kind, path, &mut Budget::default())
}

/// What one rich text value has used of its limits: the nodes counted so far, and the
/// depth of the block node whose fields are being checked. Rich text inside a block node
/// goes on from these rather than starting again (W1-R18).
#[derive(Default)]
pub(crate) struct Budget {
    nodes: usize,
    depth: usize,
}

/// `validate`, for rich text inside a block node of the text `budget` belongs to.
pub(crate) fn validate_within(
    value: &Value,
    kind: &FieldKind,
    path: &str,
    budget: &mut Budget,
) -> Result<(), Vec<FieldError>> {
    let FieldKind::RichText {
        features,
        relation_to,
        ..
    } = kind
    else {
        return Ok(());
    };
    let mut check = Check {
        kind,
        features: features.as_deref(),
        relation_to,
        budget,
        ids: HashSet::new(),
        errors: Vec::new(),
    };
    check.doc(value, path);
    if check.errors.is_empty() {
        Ok(())
    } else {
        Err(check.errors)
    }
}

/// One validation, stopping at the first node at fault.
struct Check<'a, 'b> {
    kind: &'a FieldKind,
    features: Option<&'a [Feature]>,
    relation_to: &'a [String],
    budget: &'b mut Budget,
    ids: HashSet<Uuid>,
    errors: Vec<FieldError>,
}

impl Check<'_, '_> {
    fn fail(&mut self, path: String, message: impl Into<String>) {
        self.errors.push(FieldError {
            path,
            message: message.into(),
        });
    }

    fn allows(&self, feature: Feature) -> bool {
        self.features.is_none_or(|f| f.contains(&feature))
    }

    fn disabled(&mut self, path: String, name: &str) {
        self.fail(
            path,
            format!("the `{name}` feature isn't turned on for this field"),
        );
    }

    fn doc(&mut self, value: &Value, path: &str) {
        let Some(doc) = value
            .as_object()
            .filter(|o| o.get("type").and_then(Value::as_str) == Some("doc"))
        else {
            return self.fail(path.into(), DOC_SHAPE);
        };
        if let Some(key) = doc
            .keys()
            .find(|k| !matches!(k.as_str(), "type" | "content"))
        {
            return self.fail(format!("{path}.{key}"), format!("unknown key `{key}`"));
        }
        let depth = self.budget.depth;
        self.children(doc, path, "doc", Holds::Blocks, depth, false);
    }

    /// The nodes in `node`'s `content`, which hold `holds`. `required`: at least one.
    fn children(
        &mut self,
        node: &Map<String, Value>,
        path: &str,
        parent: &str,
        holds: Holds,
        depth: usize,
        required: bool,
    ) {
        let here = format!("{path}.content");
        let children = match node.get("content") {
            None => &Vec::new(),
            Some(Value::Array(children)) => children,
            Some(_) => return self.fail(here, "expected a list of nodes"),
        };
        if required && children.is_empty() {
            return self.fail(here, "needs at least one node inside");
        }
        for (i, child) in children.iter().enumerate() {
            self.node(child, &format!("{here}.{i}"), parent, holds, depth + 1);
            if !self.errors.is_empty() {
                return;
            }
        }
    }

    fn node(&mut self, v: &Value, path: &str, parent: &str, holds: Holds, depth: usize) {
        self.budget.nodes += 1;
        if self.budget.nodes > MAX_NODES {
            return self.fail(path.into(), format!("holds more than {MAX_NODES} nodes"));
        }
        if depth > MAX_DEPTH {
            return self.fail(
                path.into(),
                format!("nests more than {MAX_DEPTH} levels deep"),
            );
        }
        let Some((node, t)) = v
            .as_object()
            .and_then(|o| Some((o, o.get("type")?.as_str()?)))
        else {
            return self.fail(path.into(), "expected a node with a type");
        };
        let Some((feature, inner, attrs)) = node_spec(t) else {
            return self.fail(path.into(), format!("unknown node type `{t}`"));
        };
        if let Some(feature) = feature
            && !self.allows(feature)
        {
            return self.disabled(path.into(), t);
        }
        if !holds.fits(t) {
            return self.fail(path.into(), format!("`{t}` can't go inside `{parent}`"));
        }
        let own = |k: &str| match k {
            "type" | "attrs" => true,
            "content" => inner != Holds::Nothing,
            "text" | "marks" => t == "text",
            _ => false,
        };
        if let Some(key) = node.keys().find(|k| !own(k)) {
            return self.fail(format!("{path}.{key}"), format!("unknown key `{key}`"));
        }
        let empty = Map::new();
        let given = match node.get("attrs") {
            None => &empty,
            Some(Value::Object(a)) => a,
            Some(_) => return self.fail(format!("{path}.attrs"), "expected an object"),
        };
        if let Some(key) = given.keys().find(|k| !attrs.contains(&k.as_str())) {
            return self.fail(
                format!("{path}.attrs.{key}"),
                format!("unknown attribute `{key}`"),
            );
        }
        self.attrs(t, given, path, depth);
        if !self.errors.is_empty() {
            return;
        }
        if t == "text" {
            if !node
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                return self.fail(format!("{path}.text"), "expected text that isn't empty");
            }
            match node.get("marks") {
                None => {}
                Some(Value::Array(m)) if m.is_empty() => {}
                Some(_) if parent == "codeBlock" => {
                    return self.fail(
                        format!("{path}.marks"),
                        "code blocks hold plain text without marks",
                    );
                }
                Some(marks) => self.marks(marks, &format!("{path}.marks")),
            }
            return;
        }
        if inner != Holds::Nothing {
            let required = matches!(inner, Holds::Blocks | Holds::Only(_));
            self.children(node, path, t, inner, depth, required);
        }
        if t == "table" && self.errors.is_empty() {
            self.table(node, path);
        }
    }

    /// A node's attributes, already known to be ones it takes. The node sits at `depth`.
    fn attrs(&mut self, t: &str, attrs: &Map<String, Value>, path: &str, depth: usize) {
        let at = |k: &str| format!("{path}.attrs.{k}");
        // An optional attribute may be missing or `null`.
        let given = |k: &str| attrs.get(k).filter(|v| !v.is_null());
        let whole =
            |k: &str, min: u64| given(k).is_none_or(|v| v.as_u64().is_some_and(|n| n >= min));
        match t {
            "heading" => match given("level").map(|v| v.as_u64().filter(|l| (1..=6).contains(l))) {
                None => self.fail(at("level"), "is required"),
                Some(None) => self.fail(at("level"), "expected a whole number from 1 to 6"),
                Some(Some(_)) => {}
            },
            "orderedList" if !whole("start", 0) => {
                self.fail(at("start"), "expected a whole number of 0 or more")
            }
            "taskItem" if given("checked").is_some_and(|v| !v.is_boolean()) => {
                self.fail(at("checked"), "expected true or false")
            }
            "codeBlock"
                if given("language").is_some_and(|v| {
                    !v.as_str().is_some_and(|l| {
                        l.len() <= 32
                            && l.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
                    })
                }) =>
            {
                self.fail(
                    at("language"),
                    "expected up to 32 letters, digits, _, + or -",
                )
            }
            "tableCell" | "tableHeader" => {
                for k in ["colspan", "rowspan"] {
                    if given(k)
                        .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=MAX_SPAN).contains(&n)))
                    {
                        return self.fail(
                            at(k),
                            format!("expected a whole number from 1 to {MAX_SPAN}"),
                        );
                    }
                }
                let span = given("colspan").and_then(Value::as_u64).unwrap_or(1);
                if given("colwidth").is_some_and(|w| {
                    !w.as_array().is_some_and(|w| {
                        w.len() as u64 == span
                            && w.iter().all(|n| n.as_u64().is_some_and(|n| n >= 1))
                    })
                }) {
                    self.fail(
                        at("colwidth"),
                        "expected a list of widths in pixels, one per column the cell spans",
                    )
                }
            }
            "relationship" => self.link(attrs, &format!("{path}.attrs")),
            "block" => self.block(attrs, path, depth),
            _ => {}
        }
    }

    /// A link to a document, `{"relationTo": "<collection>", "value": "<ID>"}`, at `path`.
    fn link(&mut self, link: &Map<String, Value>, path: &str) {
        if link.contains_key("slug") {
            return self.fail(
                path.into(),
                "names its document by slug; it must be saved with the document's ID",
            );
        }
        if let Some(key) = link
            .keys()
            .find(|k| !matches!(k.as_str(), "relationTo" | "value"))
        {
            return self.fail(format!("{path}.{key}"), format!("unknown key `{key}`"));
        }
        let at = |k: &str| format!("{path}.{k}");
        match link.get("relationTo").and_then(Value::as_str) {
            None => return self.fail(at("relationTo"), "is required"),
            Some(_) if self.relation_to.is_empty() => {
                return self.fail(at("relationTo"), "this field can't link to documents");
            }
            Some(c) if !self.relation_to.iter().any(|r| r == c) => {
                let allowed = self.relation_to.join(", ");
                return self.fail(
                    at("relationTo"),
                    format!("can't link to `{c}`; expected one of: {allowed}"),
                );
            }
            Some(_) => {}
        }
        match link.get("value") {
            None | Some(Value::Null) => self.fail(at("value"), "is required"),
            Some(v) if !v.as_str().is_some_and(|s| Uuid::try_parse(s).is_ok()) => {
                self.fail(at("value"), "expected a document ID")
            }
            Some(_) => {}
        }
    }

    /// A block node's type, ID and fields. Rich text in its fields nests below `depth`.
    fn block(&mut self, attrs: &Map<String, Value>, path: &str, depth: usize) {
        let at = |k: &str| format!("{path}.attrs.{k}");
        let kind = self.kind;
        let offered: Vec<&str> = kind
            .block_sources()
            .iter()
            .filter_map(BlockSource::def)
            .map(|b| b.slug.as_str())
            .collect();
        if offered.is_empty() {
            return self.fail(at("blockType"), "this field has no block types");
        }
        let block = match attrs.get("blockType") {
            None | Some(Value::Null) => return self.fail(at("blockType"), "is required"),
            Some(t) => match t.as_str().and_then(|slug| kind.block(slug)) {
                Some(block) => block,
                None => {
                    return self.fail(
                        at("blockType"),
                        format!("expected one of: {}", offered.join(", ")),
                    );
                }
            },
        };
        match attrs.get("id").map(|id| id.as_str().map(Uuid::try_parse)) {
            None => {}
            // Compared as UUIDs, so the same ID in another case still counts.
            Some(Some(Ok(id))) => {
                if !self.ids.insert(id) {
                    return self.fail(at("id"), "this block ID is used twice");
                }
            }
            Some(_) => return self.fail(at("id"), "expected a block ID"),
        }
        let empty = Map::new();
        let fields = match attrs.get("fields") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(f)) => f,
            Some(_) => return self.fail(at("fields"), "expected an object"),
        };
        // Every field of this node starts one level below it, so a block node
        // deeper inside one field must not carry its depth to the next.
        let outer = self.budget.depth;
        self.budget.depth = depth;
        crate::validate::check_object(
            &block.fields,
            fields,
            &[],
            &at("fields"),
            &mut self.errors,
            Some(&mut *self.budget),
        );
        self.budget.depth = outer;
    }

    fn marks(&mut self, marks: &Value, path: &str) {
        let Value::Array(marks) = marks else {
            return self.fail(path.into(), "expected a list of marks");
        };
        let mut seen = Vec::new();
        for (i, mark) in marks.iter().enumerate() {
            let here = format!("{path}.{i}");
            let Some((m, t)) = mark
                .as_object()
                .and_then(|o| Some((o, o.get("type")?.as_str()?)))
            else {
                return self.fail(here, "expected a mark with a type");
            };
            let Some(feature) = mark_feature(t) else {
                return self.fail(here, format!("unknown mark `{t}`"));
            };
            if !self.allows(feature) {
                return self.disabled(here, t);
            }
            if seen.contains(&t) {
                return self.fail(here, format!("`{t}` is applied twice"));
            }
            seen.push(t);
            if let Some(key) = m.keys().find(|k| !matches!(k.as_str(), "type" | "attrs")) {
                return self.fail(format!("{here}.{key}"), format!("unknown key `{key}`"));
            }
            let at = format!("{here}.attrs");
            let empty = Map::new();
            let attrs = match m.get("attrs") {
                None => &empty,
                Some(Value::Object(a)) => a,
                Some(_) => return self.fail(at, "expected an object"),
            };
            // A link opens in a new tab by `newTab`; raw `target`, `rel` and `class` would let
            // the text set what the page does (W1-R20).
            let takes: &[&str] = if t == "link" {
                &["href", "doc", "newTab"]
            } else {
                &[]
            };
            if let Some(key) = attrs.keys().find(|k| !takes.contains(&k.as_str())) {
                return self.fail(format!("{at}.{key}"), format!("unknown attribute `{key}`"));
            }
            if t != "link" {
                continue;
            }
            if attrs
                .get("newTab")
                .is_some_and(|v| !v.is_null() && !v.is_boolean())
            {
                return self.fail(format!("{at}.newTab"), "expected true or false");
            }
            match (attrs.get("href"), attrs.get("doc")) {
                (Some(_), Some(_)) => return self.fail(at, "a link takes href or doc, not both"),
                (None, None) => return self.fail(at, "a link needs href or doc"),
                (Some(href), None) => {
                    if !href.as_str().is_some_and(safe_href) {
                        return self.fail(format!("{at}.href"), SCHEMES);
                    }
                }
                (None, Some(Value::Object(doc))) => self.link(doc, &format!("{at}.doc")),
                (None, Some(_)) => {
                    return self.fail(
                        format!("{at}.doc"),
                        "expected {\"relationTo\": \"<collection>\", \"value\": \"<document ID>\"}",
                    );
                }
            }
            if !self.errors.is_empty() {
                return;
            }
        }
    }

    /// Every row of a table spans the same number of columns, counting cells reaching down
    /// from rows above, and no cell reaches past the last row.
    fn table(&mut self, table: &Map<String, Value>, path: &str) {
        let rows = table
            .get("content")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        // Columns taken in each row by cells from the rows above.
        let mut carried = vec![0usize; rows.len()];
        let mut first = None;
        for (r, row) in rows.iter().enumerate() {
            let mut width = carried[r];
            let cells = row
                .get("content")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for (c, cell) in cells.iter().enumerate() {
                let span = |k: &str| {
                    cell.get("attrs")
                        .and_then(|a| a.get(k))
                        .and_then(Value::as_u64)
                        .map_or(1, |n| usize::try_from(n).unwrap_or(usize::MAX))
                };
                let (across, down) = (span("colspan"), span("rowspan"));
                if down > rows.len() - r {
                    return self.fail(
                        format!("{path}.content.{r}.content.{c}.attrs.rowspan"),
                        "reaches past the last row",
                    );
                }
                width = width.saturating_add(across);
                for below in &mut carried[r + 1..r + down] {
                    *below = below.saturating_add(across);
                }
            }
            match first {
                None => first = Some(width),
                Some(w) if w != width => {
                    return self.fail(
                        format!("{path}.content.{r}"),
                        format!(
                            "every row of a table must span the same number of columns: this one spans {width}, the first {w}"
                        ),
                    );
                }
                Some(_) => {}
            }
        }
    }
}

/// Stores each link's scheme lowercased (W1-R20): `HTTPS://` as `https://`.
pub(crate) fn lower_schemes(value: &mut Value) {
    each_node_mut(value, &mut |node| {
        let Some(Value::Array(marks)) = node.get_mut("marks") else {
            return;
        };
        for href in marks
            .iter_mut()
            .filter_map(|m| m.pointer_mut("/attrs/href"))
        {
            if let Value::String(h) = href
                && let Some(i) = h.find(':')
                && h[..i].bytes().all(|b| b.is_ascii_alphabetic())
            {
                h[..i].make_ascii_lowercase();
            }
        }
    });
}

/// Whether a value holds nothing an editor typed: no nodes, or only paragraphs holding
/// nothing but whitespace and line breaks.
pub(crate) fn is_empty(value: &Value) -> bool {
    let blank = |n: &Value| match n.get("type").and_then(Value::as_str) {
        Some("hardBreak") => true,
        Some("text") => n
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(|t| t.trim().is_empty()),
        _ => false,
    };
    value
        .get("content")
        .and_then(Value::as_array)
        .is_none_or(|nodes| {
            nodes.iter().all(|n| {
                n.get("type").and_then(Value::as_str) == Some("paragraph")
                    && n.get("content")
                        .and_then(Value::as_array)
                        .is_none_or(|inline| inline.iter().all(blank))
            })
        })
}

/// Calls `f` on `node` and every node inside it, in document order.
fn each_node<'v>(node: &'v Value, f: &mut impl FnMut(&'v Value)) {
    f(node);
    for child in node
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        each_node(child, f);
    }
}

fn each_node_mut(node: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    let Value::Object(obj) = node else { return };
    f(obj);
    if let Some(Value::Array(children)) = obj.get_mut("content") {
        for child in children {
            each_node_mut(child, f);
        }
    }
}

/// What rich text refers to, in document order. The k-th is reported at `<field>.<k>` by
/// `relations`, and a block node's own links at `<field>.<k>.<path in its fields>`.
pub(crate) enum Item<'v> {
    /// A link to a document or a relationship node: `{"relationTo", "value"}`.
    Link(&'v Value),
    /// A block node's `attrs`: `{"blockType", "id", "fields"}`.
    Block(&'v Map<String, Value>),
}

pub(crate) enum ItemMut<'v> {
    Link(&'v mut Value),
    Block(&'v mut Map<String, Value>),
}

fn is_link_mark(mark: &Value) -> bool {
    mark.get("type").and_then(Value::as_str) == Some("link")
}

/// Every `Item` in `value`, in order: a text node's links, then the node itself, then
/// the nodes inside it. Each comes with where it sits in `value`, as validation names it:
/// `content.0.content.1.marks.0.attrs.doc` for a link, `content.2.attrs` for a node.
pub(crate) fn items(value: &Value) -> Vec<(String, Item<'_>)> {
    fn walk<'v>(node: &'v Value, path: &str, out: &mut Vec<(String, Item<'v>)>) {
        let at = |rest: String| match path {
            "" => rest,
            _ => format!("{path}.{rest}"),
        };
        let marks = node.get("marks").and_then(Value::as_array);
        for (m, mark) in marks.into_iter().flatten().enumerate() {
            if is_link_mark(mark)
                && let Some(doc) = mark.pointer("/attrs/doc")
            {
                out.push((at(format!("marks.{m}.attrs.doc")), Item::Link(doc)));
            }
        }
        match (node.get("type").and_then(Value::as_str), node.get("attrs")) {
            (Some("relationship"), Some(attrs)) => {
                out.push((at("attrs".into()), Item::Link(attrs)))
            }
            (Some("block"), Some(Value::Object(attrs))) => {
                out.push((at("attrs".into()), Item::Block(attrs)))
            }
            _ => {}
        }
        let children = node.get("content").and_then(Value::as_array);
        for (i, child) in children.into_iter().flatten().enumerate() {
            walk(child, &at(format!("content.{i}")), out);
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out
}

/// `items`, to change: `f` gets each with its index.
pub(crate) fn each_item_mut(value: &mut Value, f: &mut impl FnMut(usize, ItemMut<'_>)) {
    let mut k = 0;
    each_node_mut(value, &mut |node| {
        if let Some(Value::Array(marks)) = node.get_mut("marks") {
            for mark in marks.iter_mut().filter(|m| is_link_mark(m)) {
                if let Some(doc) = mark.pointer_mut("/attrs/doc") {
                    f(k, ItemMut::Link(doc));
                    k += 1;
                }
            }
        }
        let t = node.get("type").and_then(Value::as_str);
        let (relationship, block) = (t == Some("relationship"), t == Some("block"));
        match node.get_mut("attrs") {
            Some(attrs) if relationship => f(k, ItemMut::Link(attrs)),
            Some(Value::Object(attrs)) if block => f(k, ItemMut::Block(attrs)),
            _ => return,
        }
        k += 1;
    });
}

/// The `attrs` of every block node in `value`, at any depth.
pub(crate) fn blocks(value: &Value) -> Vec<&Map<String, Value>> {
    block_attrs(value)
        .into_iter()
        .filter_map(Value::as_object)
        .collect()
}

/// `blocks`, each as the `attrs` value itself.
pub(crate) fn block_attrs(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    each_node(value, &mut |node| {
        if node.get("type").and_then(Value::as_str) == Some("block")
            && let Some(attrs @ Value::Object(_)) = node.get("attrs")
        {
            out.push(attrs);
        }
    });
    out
}

/// Calls `f` on the `attrs` of every block node in `value`, at any depth.
pub(crate) fn blocks_mut(value: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    each_item_mut(value, &mut |_, item| {
        if let ItemMut::Block(attrs) = item {
            f(attrs);
        }
    });
}

/// The documents `value` links to and the relationship nodes it holds, in document order,
/// each by where its `{relationTo, value}` sits in `value`
/// (`content.0.content.1.marks.0.attrs.doc`, `content.2.attrs`).
pub fn links(value: &Value) -> Vec<(String, LinkRef)> {
    items(value)
        .into_iter()
        .filter_map(|(at, item)| {
            let Item::Link(link) = item else { return None };
            let doc = LinkRef::Doc {
                collection: link.get("relationTo")?.as_str()?.to_owned(),
                id: Uuid::try_parse(link.get("value")?.as_str()?).ok()?,
            };
            Some((at, doc))
        })
        .collect()
}

/// An `id` for each heading in `value`, in document order, worked out on read rather than
/// stored: the slug of its text, made unique within the document with `-2`, `-3`… A
/// heading with no letters or digits is `heading`.
pub fn heading_ids(value: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    each_node(value, &mut |node| {
        if node.get("type").and_then(Value::as_str) != Some("heading") {
            return;
        }
        let mut text = String::new();
        each_node(node, &mut |n| {
            text.push_str(n.get("text").and_then(Value::as_str).unwrap_or_default())
        });
        let mut base = slugify(&text);
        if base.is_empty() {
            base = "heading".into();
        }
        let mut id = base.clone();
        let mut n = 2;
        while !seen.insert(id.clone()) {
            id = format!("{base}-{n}");
            n += 1;
        }
        out.push(id);
    });
    out
}

/// A node's `type`, or "".
fn kind(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or_default()
}

/// A node's children.
fn children(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A node's attribute.
fn attr<'v>(node: &'v Value, name: &str) -> Option<&'v Value> {
    node.get("attrs")?.get(name)
}

/// A code block's language, if it's one validation allows today: a language stored before
/// the rule could otherwise add classes or break a fence.
fn code_language(node: &Value) -> Option<&str> {
    attr(node, "language").and_then(Value::as_str).filter(|l| {
        !l.is_empty()
            && l.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
    })
}

/// A block node field's value as one attribute: a string or other scalar as its text,
/// anything else as compact JSON (W1-R21). `None` for a null.
fn field_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Field;
    use crate::validate::{map_relations, relations, validate_document};
    use serde_json::{Map, json};

    pub(super) const P1: &str = "0199d5c4-0000-7000-8000-0000000000a1";
    pub(super) const P2: &str = "0199d5c4-0000-7000-8000-0000000000a2";
    pub(super) const A1: &str = "0199d5c4-0000-7000-8000-0000000000b1";
    pub(super) const A2: &str = "0199d5c4-0000-7000-8000-0000000000b2";
    pub(super) const B1: &str = "0199d5c4-0000-7000-8000-0000000000c1";

    /// A `body` rich text field with `options` on top of its type.
    fn fields(options: Value) -> Vec<Field> {
        let mut f = json!({"name": "body", "type": "richText"});
        f.as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        serde_json::from_value(json!([f])).unwrap()
    }

    /// Links to posts and authors, and a `cta` block with a required label and a link.
    fn everything() -> Vec<Field> {
        fields(json!({"relationTo": ["posts", "authors"], "blocks": [
            {"slug": "cta", "fields": [
                {"name": "label", "type": "text", "required": true},
                {"name": "author", "type": "relationship", "relationTo": "authors"},
                {"name": "meta", "type": "group", "fields": [
                    {"name": "editor", "type": "relationship", "relationTo": "authors"}
                ]}
            ]}
        ]}))
    }

    /// A `box` block type holding rich text of its own.
    fn boxed() -> Vec<Field> {
        fields(
            json!({"blocks": [{"slug": "box", "fields": [{"name": "inner", "type": "richText"}]}]}),
        )
    }

    /// A one-cell table whose cell has `attrs`.
    fn table_cell(attrs: Value) -> Value {
        json!({"type": "table", "content": [{"type": "tableRow", "content": [
            {"type": "tableCell", "attrs": attrs, "content": [{"type": "paragraph"}]}
        ]}]})
    }

    pub(super) fn doc(content: Value) -> Value {
        json!({"type": "doc", "content": content})
    }

    pub(super) fn text(s: &str) -> Value {
        json!({"type": "text", "text": s})
    }

    pub(super) fn para(content: Value) -> Value {
        json!({"type": "paragraph", "content": content})
    }

    pub(super) fn linked(attrs: Value) -> Value {
        json!({"type": "text", "text": "x", "marks": [{"type": "link", "attrs": attrs}]})
    }

    fn check(fields: &[Field], body: Value) -> Result<Map<String, Value>, Vec<FieldError>> {
        validate_document(fields, &json!({ "body": body }), None)
    }

    /// `depth` blockquotes, one inside the next, around a paragraph: the paragraph sits at
    /// `depth + 1` and its text at `depth + 2`.
    fn nested(depth: usize) -> Value {
        let mut node = para(json!([text("deep")]));
        for _ in 0..depth {
            node = json!({"type": "blockquote", "content": [node]});
        }
        doc(json!([node]))
    }

    /// One document using every node and mark: links to post `P1`, a relationship node to
    /// author `A1`, and `cta` block nodes from `everything`.
    pub(super) fn full() -> Value {
        let cell = |kind: &str, attrs: Value| json!({"type": kind, "attrs": attrs, "content": [para(json!([text("c")]))]});
        doc(json!([
            {"type": "heading", "attrs": {"level": 2}, "content": [text("Title")]},
            para(json!([
                {"type": "text", "text": "b", "marks": [{"type": "bold"}]},
                {"type": "text", "text": "i", "marks": [
                    {"type": "italic"}, {"type": "underline"}, {"type": "strike"}
                ]},
                {"type": "text", "text": "s", "marks": [{"type": "subscript"}]},
                {"type": "text", "text": "S", "marks": [{"type": "superscript"}, {"type": "code"}]},
                {"type": "hardBreak"},
                linked(json!({"href": "https://example.com/a?b=c#d"})),
                linked(json!({"href": "http://example.com"})),
                linked(json!({"href": "mailto:ann@example.com"})),
                linked(json!({"href": "tel:+441234567890"})),
                linked(json!({"href": "/about"})),
                linked(json!({"href": "#top"})),
                linked(json!({"doc": {"relationTo": "posts", "value": P1}})),
                linked(json!({"href": "/new", "newTab": true})),
                linked(json!({"doc": {"relationTo": "posts", "value": P1}, "newTab": false})),
                linked(json!({"href": "/same", "newTab": null}))
            ])),
            {"type": "paragraph"},
            {"type": "bulletList", "content": [
                {"type": "listItem", "content": [
                    para(json!([text("one")])),
                    {"type": "orderedList", "attrs": {"start": 3}, "content": [
                        {"type": "listItem", "content": [para(json!([text("three")]))]}
                    ]}
                ]}
            ]},
            {"type": "orderedList", "content": [{"type": "listItem", "content": [para(json!([]))]}]},
            {"type": "taskList", "content": [
                {"type": "taskItem", "attrs": {"checked": true}, "content": [para(json!([text("done")]))]},
                {"type": "taskItem", "content": [para(json!([text("to do")]))]}
            ]},
            {"type": "blockquote", "content": [para(json!([text("quoted")]))]},
            {"type": "horizontalRule"},
            {"type": "codeBlock", "attrs": {"language": "c++"}, "content": [text("int main() {}")]},
            {"type": "codeBlock", "attrs": {"language": null}},
            {"type": "table", "content": [
                {"type": "tableRow", "content": [cell("tableHeader", json!({"colspan": 2}))]},
                {"type": "tableRow", "content": [
                    cell("tableCell", json!({})),
                    cell("tableCell", json!({"rowspan": 2, "colwidth": [120]}))
                ]},
                {"type": "tableRow", "content": [cell("tableCell", json!({"colspan": 1, "rowspan": 1}))]}
            ]},
            {"type": "relationship", "attrs": {"relationTo": "authors", "value": A1}},
            {"type": "block", "attrs": {"blockType": "cta", "id": B1, "fields": {
                "label": "Go", "author": A2, "meta": {"editor": A1}
            }}},
            {"type": "block", "attrs": {"blockType": "cta", "fields": {"label": "New"}}}
        ]))
    }

    #[test]
    fn rich_text_accepts_the_full_node_set() {
        let body = full();
        assert_eq!(validate(&body, &everything()[0].kind, "body"), Ok(()));
        let stored = check(&everything(), body.clone()).unwrap();
        // A new block node gets an ID; everything else is stored as sent.
        let nodes = stored["body"]["content"].as_array().unwrap();
        let new_block = &nodes[nodes.len() - 1]["attrs"];
        assert!(Uuid::try_parse(new_block["id"].as_str().unwrap()).is_ok());
        assert_eq!(nodes[nodes.len() - 2], body["content"][nodes.len() - 2]);
        assert_eq!(
            nodes[..nodes.len() - 1],
            body["content"].as_array().unwrap()[..nodes.len() - 1]
        );

        // Schemes compare in any case and are stored lowercased (W1-R20).
        let shouting = |h: &str| doc(json!([para(json!([linked(json!({ "href": h }))]))]));
        for (sent, kept) in [
            ("HTTPS://Example.com/A", "https://Example.com/A"),
            ("MailTo:Ann@example.com", "mailto:Ann@example.com"),
            ("/Path:Colon", "/Path:Colon"),
            ("#Top:1", "#Top:1"),
        ] {
            let stored = check(&everything(), shouting(sent)).unwrap();
            assert_eq!(
                stored["body"].pointer("/content/0/content/0/marks/0/attrs/href"),
                Some(&json!(kept))
            );
        }

        // An empty document is fine; a required field needs something in it.
        assert!(check(&everything(), doc(json!([]))).is_ok());
        assert!(check(&everything(), json!({"type": "doc"})).is_ok());
        let required = fields(json!({"required": true}));
        for empty in [
            doc(json!([])),
            doc(json!([{"type": "paragraph"}])),
            doc(json!([para(
                json!([text("  "), {"type": "hardBreak"}, text("\t")])
            )])),
        ] {
            assert_eq!(
                check(&required, empty).unwrap_err()[0].message,
                "is required"
            );
        }
        assert!(check(&required, doc(json!([para(json!([text("hi")]))]))).is_ok());
        // A value that isn't a document is told so, not that it's required.
        for bad in [
            json!({"type": "paragraph"}),
            json!({}),
            json!({"type": "bogus", "content": []}),
            json!("  "),
        ] {
            assert_eq!(check(&required, bad).unwrap_err()[0].message, DOC_SHAPE);
        }

        // As deep as the limit allows: the text sits 32 levels down.
        assert!(check(&everything(), nested(30)).is_ok());
        // As many nodes as the limit allows.
        let rules: Vec<Value> = (0..50_000)
            .map(|_| json!({"type": "horizontalRule"}))
            .collect();
        assert!(check(&everything(), doc(json!(rules))).is_ok());

        // Block nodes get their fields' defaults as block rows do, on create and update.
        let toned = fields(json!({"blocks": [{"slug": "cta", "fields": [
            {"name": "tone", "type": "text", "default": "calm"}
        ]}]}));
        let bare = doc(json!([{"type": "block", "attrs": {"blockType": "cta"}}]));
        let created = check(&toned, bare.clone()).unwrap();
        assert_eq!(
            created["body"]["content"][0]["attrs"]["fields"],
            json!({"tone": "calm"})
        );
        let updated =
            validate_document(&toned, &json!({ "body": bare }), Some(&Map::new())).unwrap();
        assert_eq!(
            updated["body"]["content"][0]["attrs"]["fields"],
            json!({"tone": "calm"})
        );

        // Only what `features` names is allowed, plus paragraphs and text.
        let plain = fields(json!({"features": ["bold"]}));
        assert!(
            check(
                &plain,
                doc(json!([para(
                    json!([{"type": "text", "text": "b", "marks": [{"type": "bold"}]}])
                )]))
            )
            .is_ok()
        );
    }

    #[test]
    fn sibling_rich_text_in_a_block_starts_from_the_block_depth() {
        // A deep block node in `a` must not push `b` deeper: both fields of the
        // outer block start one level below it.
        let fields = fields(json!({"blocks": [{"slug": "pair", "fields": [
            {"name": "a", "type": "richText", "blocks": [{"slug": "leaf", "fields": []}]},
            {"name": "b", "type": "richText"}
        ]}]}));
        let mut deep_block = json!({"type": "block", "attrs": {"blockType": "leaf"}});
        for _ in 0..20 {
            deep_block = json!({"type": "blockquote", "content": [deep_block]});
        }
        let body = doc(
            json!([{"type": "block", "attrs": {"blockType": "pair", "fields": {
                "a": doc(json!([deep_block])),
                "b": nested(25)
            }}}]),
        );
        assert_eq!(check(&fields, body).err(), None);
    }

    #[test]
    fn rich_text_refuses_with_paths() {
        let href = |h: &str| doc(json!([para(json!([linked(json!({ "href": h }))]))]));
        let at_href = "body.content.0.content.0.marks.0.attrs.href";
        let scheme = "links must start with http://, https://, mailto:, tel:, / or #";
        let mut cases: Vec<(Vec<Field>, Value, &str, String)> = Vec::new();
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "\u{0}javascript:alert(1)",
            " javascript:alert(1)",
            "&#106;avascript:alert(1)",
            "&#x6A;avascript:alert(1)",
            "data:text/html;base64,PHNjcmlwdD4=",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
            "ftp://example.com",
            "//evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "https://exa mple.com",
            "https:\t//example.com",
            "",
        ] {
            cases.push((everything(), href(bad), at_href, scheme.into()));
        }
        let all = everything;
        let deep = format!("body{}", ".content.0".repeat(33));
        let deep_inner = format!(
            "body.content.0.attrs.fields.inner{}",
            ".content.0".repeat(32)
        );
        let one = |node: Value| doc(json!([node]));
        let disabled = |name: &str| format!("the `{name}` feature isn't turned on for this field");
        let narrow = || fields(json!({"features": ["bold", "link"], "relationTo": ["posts"]}));
        cases.extend([
            // Features.
            (
                narrow(),
                one(json!({"type": "table", "content": []})),
                "body.content.0",
                disabled("table"),
            ),
            (
                narrow(),
                one(para(json!([{"type": "text", "text": "x", "marks": [{"type": "italic"}]}]))),
                "body.content.0.content.0.marks.0",
                disabled("italic"),
            ),
            (
                narrow(),
                one(json!({"type": "relationship", "attrs": {"relationTo": "posts", "value": P1}})),
                "body.content.0",
                disabled("relationship"),
            ),
            (
                narrow(),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "fields": {"label": "x"}}})),
                "body.content.0",
                disabled("block"),
            ),
            (
                narrow(),
                one(para(json!([{"type": "hardBreak"}]))),
                "body.content.0.content.0",
                disabled("hardBreak"),
            ),
            (
                narrow(),
                one(json!({"type": "bulletList", "content": [
                    {"type": "listItem", "content": [para(json!([]))]}
                ]})),
                "body.content.0",
                disabled("bulletList"),
            ),
            // Unknown nodes, marks, keys and attributes.
            (all(), one(json!({"type": "video"})), "body.content.0", "unknown node type `video`".into()),
            (all(), one(json!({"type": "upload", "attrs": {}})), "body.content.0", "unknown node type `upload`".into()),
            (all(), one(json!({"content": []})), "body.content.0", "expected a node with a type".into()),
            (all(), one(json!("text")), "body.content.0", "expected a node with a type".into()),
            (
                all(),
                one(para(json!([{"type": "text", "text": "x", "marks": [{"type": "blink"}]}]))),
                "body.content.0.content.0.marks.0",
                "unknown mark `blink`".into(),
            ),
            (
                all(),
                one(json!({"type": "heading", "attrs": {"level": 1, "id": "x"}})),
                "body.content.0.attrs.id",
                "unknown attribute `id`".into(),
            ),
            (
                all(),
                one(json!({"type": "paragraph", "attrs": {"textAlign": "left"}})),
                "body.content.0.attrs.textAlign",
                "unknown attribute `textAlign`".into(),
            ),
            (
                all(),
                one(json!({"type": "paragraph", "style": "x"})),
                "body.content.0.style",
                "unknown key `style`".into(),
            ),
            (
                all(),
                one(json!({"type": "horizontalRule", "content": []})),
                "body.content.0.content",
                "unknown key `content`".into(),
            ),
            (
                all(),
                one(para(json!([{"type": "text", "text": "x", "marks": [
                    {"type": "bold", "attrs": {"weight": 700}}
                ]}]))),
                "body.content.0.content.0.marks.0.attrs.weight",
                "unknown attribute `weight`".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({"href": "/a", "target": "_blank"}))]))),
                "body.content.0.content.0.marks.0.attrs.target",
                "unknown attribute `target`".into(),
            ),
            (
                all(),
                one(para(json!([{"type": "text", "text": "x", "marks": [{"type": "bold"}, {"type": "bold"}]}]))),
                "body.content.0.content.0.marks.1",
                "`bold` is applied twice".into(),
            ),
            // Nesting.
            (
                all(),
                one(json!({"type": "listItem", "content": [para(json!([]))]})),
                "body.content.0",
                "`listItem` can't go inside `doc`".into(),
            ),
            (
                all(),
                one(json!({"type": "bulletList", "content": [para(json!([]))]})),
                "body.content.0.content.0",
                "`paragraph` can't go inside `bulletList`".into(),
            ),
            (all(), one(text("loose")), "body.content.0", "`text` can't go inside `doc`".into()),
            (
                all(),
                one(para(json!([para(json!([]))]))),
                "body.content.0.content.0",
                "`paragraph` can't go inside `paragraph`".into(),
            ),
            (
                all(),
                one(json!({"type": "bulletList", "content": []})),
                "body.content.0.content",
                "needs at least one node inside".into(),
            ),
            (
                all(),
                one(json!({"type": "codeBlock", "content": [{"type": "hardBreak"}]})),
                "body.content.0.content.0",
                "`hardBreak` can't go inside `codeBlock`".into(),
            ),
            (
                all(),
                one(json!({"type": "codeBlock", "content": [
                    {"type": "text", "text": "x", "marks": [{"type": "bold"}]}
                ]})),
                "body.content.0.content.0.marks",
                "code blocks hold plain text without marks".into(),
            ),
            (
                all(),
                one(json!({"type": "table", "content": [
                    {"type": "tableRow", "content": [
                        {"type": "tableCell", "content": [para(json!([]))]},
                        {"type": "tableCell", "content": [para(json!([]))]}
                    ]},
                    {"type": "tableRow", "content": [{"type": "tableCell", "content": [para(json!([]))]}]}
                ]})),
                "body.content.0.content.1",
                "every row of a table must span the same number of columns: this one spans 1, the first 2".into(),
            ),
            (
                all(),
                one(json!({"type": "table", "content": [
                    {"type": "tableRow", "content": [
                        {"type": "tableCell", "attrs": {"rowspan": 2}, "content": [para(json!([]))]}
                    ]}
                ]})),
                "body.content.0.content.0.content.0.attrs.rowspan",
                "reaches past the last row".into(),
            ),
            // Attributes.
            (
                all(),
                one(json!({"type": "heading", "attrs": {"level": 7}})),
                "body.content.0.attrs.level",
                "expected a whole number from 1 to 6".into(),
            ),
            (
                all(),
                one(json!({"type": "heading"})),
                "body.content.0.attrs.level",
                "is required".into(),
            ),
            (
                all(),
                one(json!({"type": "codeBlock", "attrs": {"language": "js\" onload=\"x"}})),
                "body.content.0.attrs.language",
                "expected up to 32 letters, digits, _, + or -".into(),
            ),
            (
                all(),
                one(json!({"type": "orderedList", "attrs": {"start": -1}, "content": [
                    {"type": "listItem", "content": [para(json!([]))]}
                ]})),
                "body.content.0.attrs.start",
                "expected a whole number of 0 or more".into(),
            ),
            (
                all(),
                one(json!({"type": "taskList", "content": [
                    {"type": "taskItem", "attrs": {"checked": "yes"}, "content": [para(json!([]))]}
                ]})),
                "body.content.0.content.0.attrs.checked",
                "expected true or false".into(),
            ),
            (all(), one(para(json!([text("")]))), "body.content.0.content.0.text", "expected text that isn't empty".into()),
            (
                all(),
                one(para(json!([linked(json!({"href": "/a", "newTab": "yes"}))]))),
                "body.content.0.content.0.marks.0.attrs.newTab",
                "expected true or false".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({"href": "/a", "rel": "opener"}))]))),
                "body.content.0.content.0.marks.0.attrs.rel",
                "unknown attribute `rel`".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({"href": "/a", "class": "x"}))]))),
                "body.content.0.content.0.marks.0.attrs.class",
                "unknown attribute `class`".into(),
            ),
            // Spans and widths are bounded.
            (
                all(),
                one(table_cell(json!({"colspan": 1_000_000_000_000_000_000u64}))),
                "body.content.0.content.0.content.0.attrs.colspan",
                "expected a whole number from 1 to 1000".into(),
            ),
            (
                all(),
                one(table_cell(json!({"rowspan": 1001}))),
                "body.content.0.content.0.content.0.attrs.rowspan",
                "expected a whole number from 1 to 1000".into(),
            ),
            (
                all(),
                one(table_cell(json!({"colwidth": [0]}))),
                "body.content.0.content.0.content.0.attrs.colwidth",
                "expected a list of widths in pixels, one per column the cell spans".into(),
            ),
            // Limits.
            (
                all(),
                nested(31),
                &deep,
                "nests more than 32 levels deep".into(),
            ),
            (
                all(),
                doc(json!((0..50_001).map(|_| json!({"type": "horizontalRule"})).collect::<Vec<_>>())),
                "body.content.50000",
                "holds more than 50000 nodes".into(),
            ),
            // Rich text in a block node counts toward the limits of the text it's in
            // (W1-R18): its nodes sit one level below the block node.
            (
                boxed(),
                one(json!({"type": "block", "attrs": {"blockType": "box", "fields": {"inner": nested(30)}}})),
                &deep_inner,
                "nests more than 32 levels deep".into(),
            ),
            (
                boxed(),
                one(json!({"type": "block", "attrs": {"blockType": "box", "fields": {"inner":
                    doc(json!((0..50_000).map(|_| json!({"type": "horizontalRule"})).collect::<Vec<_>>()))
                }}})),
                "body.content.0.attrs.fields.inner.content.49999",
                "holds more than 50000 nodes".into(),
            ),
            // Links.
            (
                all(),
                one(para(json!([linked(json!({"href": "/a", "doc": {"relationTo": "posts", "value": P1}}))]))),
                "body.content.0.content.0.marks.0.attrs",
                "a link takes href or doc, not both".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({}))]))),
                "body.content.0.content.0.marks.0.attrs",
                "a link needs href or doc".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({"doc": {"relationTo": "posts", "slug": "hello"}}))]))),
                "body.content.0.content.0.marks.0.attrs.doc",
                "names its document by slug; it must be saved with the document's ID".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({"doc": {"relationTo": "secrets", "value": P1}}))]))),
                "body.content.0.content.0.marks.0.attrs.doc.relationTo",
                "can't link to `secrets`; expected one of: posts, authors".into(),
            ),
            (
                all(),
                one(para(json!([linked(json!({"doc": {"relationTo": "posts", "value": "nope"}}))]))),
                "body.content.0.content.0.marks.0.attrs.doc.value",
                "expected a document ID".into(),
            ),
            (
                fields(json!({})),
                one(para(json!([linked(json!({"doc": {"relationTo": "posts", "value": P1}}))]))),
                "body.content.0.content.0.marks.0.attrs.doc.relationTo",
                "this field can't link to documents".into(),
            ),
            // Relationship nodes.
            (
                all(),
                one(json!({"type": "relationship", "attrs": {"relationTo": "secrets", "value": P1}})),
                "body.content.0.attrs.relationTo",
                "can't link to `secrets`; expected one of: posts, authors".into(),
            ),
            (
                all(),
                one(json!({"type": "relationship", "attrs": {"relationTo": "posts"}})),
                "body.content.0.attrs.value",
                "is required".into(),
            ),
            // Block nodes.
            (
                all(),
                one(json!({"type": "block", "attrs": {"blockType": "hero", "fields": {}}})),
                "body.content.0.attrs.blockType",
                "expected one of: cta".into(),
            ),
            (
                all(),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "fields": {"label": 3}}})),
                "body.content.0.attrs.fields.label",
                "expected text".into(),
            ),
            (
                all(),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "fields": {}}})),
                "body.content.0.attrs.fields.label",
                "is required".into(),
            ),
            (
                all(),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "fields": {"label": "x", "stray": 1}}})),
                "body.content.0.attrs.fields.stray",
                "unknown field".into(),
            ),
            (
                all(),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "fields": {"label": "x", "author": "nope"}}})),
                "body.content.0.attrs.fields.author",
                "expected a document ID".into(),
            ),
            (
                all(),
                doc(json!([
                    {"type": "block", "attrs": {"blockType": "cta", "id": B1, "fields": {"label": "x"}}},
                    {"type": "block", "attrs": {"blockType": "cta", "id": B1.to_uppercase(), "fields": {"label": "y"}}}
                ])),
                "body.content.1.attrs.id",
                "this block ID is used twice".into(),
            ),
            (
                all(),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "id": 7, "fields": {"label": "x"}}})),
                "body.content.0.attrs.id",
                "expected a block ID".into(),
            ),
            (
                fields(json!({})),
                one(json!({"type": "block", "attrs": {"blockType": "cta", "fields": {}}})),
                "body.content.0.attrs.blockType",
                "this field has no block types".into(),
            ),
            // The document itself.
            (
                all(),
                json!({"type": "paragraph"}),
                "body",
                "expected a rich text document like {\"type\": \"doc\", \"content\": []}".into(),
            ),
            (all(), json!("<p>hi</p>"), "body", "expected a rich text document like {\"type\": \"doc\", \"content\": []}".into()),
            (all(), json!({"type": "doc", "content": {}}), "body.content", "expected a list of nodes".into()),
        ]);
        for (fields, body, path, message) in cases {
            let errors = check(&fields, body.clone()).expect_err(&format!("accepted {body}"));
            let first = &errors[0];
            assert_eq!(
                (first.path.as_str(), first.message.as_str()),
                (path, message.as_str()),
                "for {}",
                body.to_string().chars().take(300).collect::<String>()
            );
        }
    }

    #[test]
    fn rich_text_links_are_relations() {
        let fields = everything();
        let body = doc(json!([
            para(json!([
                text("see "),
                linked(json!({"doc": {"relationTo": "posts", "value": P1}})),
                linked(json!({"href": "https://example.com"}))
            ])),
            {"type": "relationship", "attrs": {"relationTo": "authors", "value": A1}},
            {"type": "blockquote", "content": [
                {"type": "block", "attrs": {"blockType": "cta", "id": B1, "fields": {
                    "label": "Go", "author": A2, "meta": {"editor": A1}
                }}}
            ]},
            para(json!([linked(json!({"doc": {"relationTo": "posts", "value": P2}}))]))
        ]));
        let stored = check(&fields, body).unwrap();
        let found: Vec<(String, String, String)> = relations(&fields, &stored)
            .into_iter()
            .map(|r| (r.path, r.relation_to, r.id.to_string()))
            .collect();
        // Errors about a link name its node, as validation does.
        let at: Vec<String> = relations(&fields, &stored)
            .into_iter()
            .map(|r| r.at)
            .collect();
        assert_eq!(
            at,
            [
                "body.content.0.content.1.marks.0.attrs.doc.value",
                "body.content.1.attrs.value",
                "body.content.2.content.0.attrs.fields.author",
                "body.content.2.content.0.attrs.fields.meta.editor",
                "body.content.3.content.0.marks.0.attrs.doc.value",
            ]
        );
        let row = |p: &str, c: &str, id: &str| (p.to_owned(), c.to_owned(), id.to_owned());
        assert_eq!(
            found,
            [
                row("body.0", "posts", P1),
                row("body.1", "authors", A1),
                row("body.2.author", "authors", A2),
                row("body.2.meta.editor", "authors", A1),
                row("body.3", "posts", P2),
            ]
        );
        let doc_ref = |c: &str, id: &str| LinkRef::Doc {
            collection: c.into(),
            id: Uuid::try_parse(id).unwrap(),
        };
        assert_eq!(
            links(&stored["body"]),
            [
                (
                    "content.0.content.1.marks.0.attrs.doc".to_owned(),
                    doc_ref("posts", P1)
                ),
                ("content.1.attrs".to_owned(), doc_ref("authors", A1)),
                (
                    "content.3.content.0.marks.0.attrs.doc".to_owned(),
                    doc_ref("posts", P2)
                ),
            ]
        );
        // The two walkers count the same references in the same order, or `relations` and
        // `map_relations` would name different things `body.<k>`.
        let kind = |item: &Item| matches!(item, Item::Block(_));
        let read: Vec<bool> = items(&stored["body"])
            .iter()
            .map(|(_, i)| kind(i))
            .collect();
        let mut written = Vec::new();
        each_item_mut(&mut stored["body"].clone(), &mut |k, item| {
            written.push((k, matches!(item, ItemMut::Block(_))))
        });
        assert_eq!(written, read.into_iter().enumerate().collect::<Vec<_>>());

        // Populating swaps each `value` for the document; a hidden one stays an ID.
        let mut populated = stored.clone();
        let mut asked = Vec::new();
        map_relations(&fields, &mut populated, &mut |path, id| {
            asked.push(path.to_owned());
            (id != P2).then(|| json!({"id": id, "path": path}))
        });
        assert_eq!(
            asked,
            [
                "body.0",
                "body.1",
                "body.2.author",
                "body.2.meta.editor",
                "body.3"
            ]
        );
        let body = &populated["body"]["content"];
        assert_eq!(
            body[0]["content"][1]["marks"][0]["attrs"]["doc"],
            json!({"relationTo": "posts", "value": {"id": P1, "path": "body.0"}})
        );
        assert_eq!(
            body[1]["attrs"],
            json!({"relationTo": "authors", "value": {"id": A1, "path": "body.1"}})
        );
        let block = &body[2]["content"][0]["attrs"]["fields"];
        assert_eq!(block["author"], json!({"id": A2, "path": "body.2.author"}));
        assert_eq!(block["meta"]["editor"]["path"], "body.2.meta.editor");
        assert_eq!(
            body[3]["content"][0]["marks"][0]["attrs"]["doc"]["value"],
            P2
        );
    }

    #[test]
    fn heading_ids_are_slugs_made_unique() {
        let heading =
            |t: &str| json!({"type": "heading", "attrs": {"level": 2}, "content": [text(t)]});
        let body = doc(json!([
            heading("Getting started"),
            para(json!([text("x")])),
            heading("Getting started"),
            {"type": "heading", "attrs": {"level": 3}, "content": [
                text("Getting "), {"type": "hardBreak"}, text("started-2")
            ]},
            heading("!!!"),
            {"type": "blockquote", "content": [heading("Getting started")]}
        ]));
        assert_eq!(
            heading_ids(&body),
            [
                "getting-started",
                "getting-started-2",
                "getting-started-2-2",
                "heading",
                "getting-started-3"
            ]
        );
    }
}
