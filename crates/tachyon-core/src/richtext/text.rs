//! Rich text as plain text: the words, without formatting, for excerpts and search.

use serde_json::Value;

use super::{RenderCtx, attr, children, kind};

/// `doc`'s text: blocks apart by a blank line, list items and table rows a line each, table
/// cells apart by a tab. Links are their text; relationship nodes and rules leave nothing;
/// a block node gives the text of the rich text in its fields; an upload node its file's
/// alt text, or else its name, unless the caller can't read it.
pub fn to_text(doc: &Value, ctx: &RenderCtx) -> String {
    joined(children(doc), "\n\n", ctx)
}

/// The text of each node that has any, apart by `sep`.
fn joined(nodes: &[Value], sep: &str, ctx: &RenderCtx) -> String {
    let parts: Vec<String> = nodes
        .iter()
        .map(|n| node_text(n, ctx))
        .filter(|t| !t.is_empty())
        .collect();
    parts.join(sep)
}

fn node_text(node: &Value, ctx: &RenderCtx) -> String {
    match kind(node) {
        "text" => node
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        "hardBreak" => "\n".into(),
        "paragraph" | "heading" | "codeBlock" => {
            children(node).iter().map(|n| node_text(n, ctx)).collect()
        }
        "blockquote" => joined(children(node), "\n\n", ctx),
        "bulletList" | "orderedList" | "taskList" | "listItem" | "taskItem" | "table" => {
            joined(children(node), "\n", ctx)
        }
        "tableRow" => children(node)
            .iter()
            .map(|n| node_text(n, ctx))
            .collect::<Vec<_>>()
            .join("\t"),
        "tableCell" | "tableHeader" => joined(children(node), " ", ctx),
        "block" => {
            let fields = attr(node, "fields").and_then(Value::as_object);
            let texts: Vec<String> = fields
                .into_iter()
                .flat_map(|f| f.values())
                .filter(|v| kind(v) == "doc")
                .map(|t| to_text(t, ctx))
                .filter(|t| !t.is_empty())
                .collect();
            texts.join("\n\n")
        }
        "upload" => super::upload_of(node, ctx)
            .map(|f| f.alt.filter(|a| !a.is_empty()).unwrap_or(f.filename))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{full, no_file};
    use super::*;

    #[test]
    fn text_renders_every_node() {
        let none = |_: &str, _: &Value| None;
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &none,
            upload: &no_file,
        };
        assert_eq!(
            to_text(&full(), &ctx),
            "Title\n\nbisS\nxxxxxxxxxx\n\none\nthree\n\ndone\nto do\n\nquoted\n\nint main() {}\n\nc\nc\tc\nc"
        );
    }
}
