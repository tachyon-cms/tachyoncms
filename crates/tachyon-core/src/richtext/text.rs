//! Rich text as plain text: the words, without formatting, for excerpts and search.

use serde_json::Value;

use super::{attr, children, kind};

/// `doc`'s text: blocks apart by a blank line, list items and table rows a line each, table
/// cells apart by a tab. Links are their text; relationship nodes and rules leave nothing;
/// a block node gives the text of the rich text in its fields.
pub fn to_text(doc: &Value) -> String {
    joined(children(doc), "\n\n")
}

/// The text of each node that has any, apart by `sep`.
fn joined(nodes: &[Value], sep: &str) -> String {
    let parts: Vec<String> = nodes
        .iter()
        .map(node_text)
        .filter(|t| !t.is_empty())
        .collect();
    parts.join(sep)
}

fn node_text(node: &Value) -> String {
    match kind(node) {
        "text" => node
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        "hardBreak" => "\n".into(),
        "paragraph" | "heading" | "codeBlock" => children(node).iter().map(node_text).collect(),
        "blockquote" => joined(children(node), "\n\n"),
        "bulletList" | "orderedList" | "taskList" | "listItem" | "taskItem" | "table" => {
            joined(children(node), "\n")
        }
        "tableRow" => children(node)
            .iter()
            .map(node_text)
            .collect::<Vec<_>>()
            .join("\t"),
        "tableCell" | "tableHeader" => joined(children(node), " "),
        "block" => {
            let fields = attr(node, "fields").and_then(Value::as_object);
            let texts: Vec<String> = fields
                .into_iter()
                .flat_map(|f| f.values())
                .filter(|v| kind(v) == "doc")
                .map(to_text)
                .filter(|t| !t.is_empty())
                .collect();
            texts.join("\n\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::full;
    use super::*;

    #[test]
    fn text_renders_every_node() {
        assert_eq!(
            to_text(&full()),
            "Title\n\nbisS\nxxxxxxxxxx\n\none\nthree\n\ndone\nto do\n\nquoted\n\nint main() {}\n\nc\nc\tc\nc"
        );
    }
}
