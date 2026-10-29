//! Rich text as HTML: escaped, with heading anchors and links that can only go somewhere safe.

use std::fmt::Write;

use serde_json::Value;

use super::{RenderCtx, attr, children, field_text, heading_ids, kind, safe_href};

/// `doc` as HTML. Text and attribute values are escaped, every `href` is checked again as
/// it's written (stored data can outlive a rule), and an internal link the context can't
/// give a URL for is its text alone. Block nodes are `<div data-block="…">` with their
/// fields as `data-*` attributes; relationship nodes are `<div data-relationship>` around
/// the linked document's title.
pub fn to_html(doc: &Value, ctx: &RenderCtx) -> String {
    let mut out = String::new();
    let mut ids = heading_ids(doc).into_iter();
    for node in children(doc) {
        node_html(node, ctx, &mut ids, &mut out);
    }
    out
}

/// `s` safe to put in text or a quoted attribute.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `fieldName` as `data-field-name`; `None` for a name that isn't plain letters, digits
/// and underscores, which a valid schema never has.
fn data_name(name: &str) -> Option<String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let mut out = String::from("data-");
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    Some(out)
}

fn node_html(
    node: &Value,
    ctx: &RenderCtx,
    ids: &mut impl Iterator<Item = String>,
    out: &mut String,
) {
    let inner = |ids: &mut _, out: &mut String| {
        for child in children(node) {
            node_html(child, ctx, ids, out);
        }
    };
    let wrap = |tag: &str, ids: &mut _, out: &mut String| {
        let _ = write!(out, "<{tag}>");
        inner(ids, out);
        let _ = write!(out, "</{tag}>");
    };
    let number = |name| attr(node, name).and_then(Value::as_u64);
    match kind(node) {
        "text" => text_html(node, ctx, out),
        "paragraph" => wrap("p", ids, out),
        "heading" => {
            let level = number("level").unwrap_or(1).clamp(1, 6);
            let id = ids.next().unwrap_or_default();
            let _ = write!(out, "<h{level} id=\"{}\">", esc(&id));
            inner(ids, out);
            let _ = write!(out, "</h{level}>");
        }
        "bulletList" => wrap("ul", ids, out),
        "orderedList" => match number("start").filter(|&s| s != 1) {
            Some(start) => {
                let _ = write!(out, "<ol start=\"{start}\">");
                inner(ids, out);
                out.push_str("</ol>");
            }
            None => wrap("ol", ids, out),
        },
        "listItem" => wrap("li", ids, out),
        "taskList" => {
            out.push_str("<ul data-task-list>");
            inner(ids, out);
            out.push_str("</ul>");
        }
        "taskItem" => {
            let checked = attr(node, "checked") == Some(&Value::Bool(true));
            let _ = write!(out, "<li data-checked=\"{checked}\">");
            inner(ids, out);
            out.push_str("</li>");
        }
        "blockquote" => wrap("blockquote", ids, out),
        "horizontalRule" => out.push_str("<hr>"),
        "hardBreak" => out.push_str("<br>"),
        "codeBlock" => {
            match attr(node, "language").and_then(Value::as_str) {
                Some(lang) if !lang.is_empty() => {
                    let _ = write!(out, "<pre><code class=\"language-{}\">", esc(lang));
                }
                _ => out.push_str("<pre><code>"),
            }
            for child in children(node) {
                out.push_str(&esc(child
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")));
            }
            out.push_str("</code></pre>");
        }
        "table" => wrap("table", ids, out),
        "tableRow" => wrap("tr", ids, out),
        cell @ ("tableCell" | "tableHeader") => {
            let tag = if cell == "tableCell" { "td" } else { "th" };
            out.push('<');
            out.push_str(tag);
            for span in ["colspan", "rowspan"] {
                if let Some(n) = number(span).filter(|&n| n > 1) {
                    let _ = write!(out, " {span}=\"{n}\"");
                }
            }
            out.push('>');
            inner(ids, out);
            let _ = write!(out, "</{tag}>");
        }
        "relationship" => {
            let to = attr(node, "relationTo").and_then(Value::as_str);
            let value = attr(node, "value");
            if let Some(title) = to.zip(value).and_then(|(to, v)| (ctx.title)(to, v)) {
                let _ = write!(out, "<div data-relationship>{}</div>", esc(&title));
            }
        }
        "block" => {
            let block_type = attr(node, "blockType").and_then(Value::as_str);
            let _ = write!(out, "<div data-block=\"{}\"", esc(block_type.unwrap_or("")));
            let fields = attr(node, "fields").and_then(Value::as_object);
            for (name, value) in fields.into_iter().flatten() {
                if let (Some(name), Some(text)) = (data_name(name), field_text(value)) {
                    let _ = write!(out, " {name}=\"{}\"", esc(&text));
                }
            }
            out.push_str("></div>");
        }
        // Validation lets nothing else in; anything else is left out rather than guessed at.
        _ => {}
    }
}

/// A text node inside its marks, the first mark outermost.
fn text_html(node: &Value, ctx: &RenderCtx, out: &mut String) {
    let marks = node.get("marks").and_then(Value::as_array);
    let mut close = Vec::new();
    for mark in marks.into_iter().flatten() {
        let tag = match mark.get("type").and_then(Value::as_str) {
            Some("bold") => "strong",
            Some("italic") => "em",
            Some("underline") => "u",
            Some("strike") => "s",
            Some("subscript") => "sub",
            Some("superscript") => "sup",
            Some("code") => "code",
            Some("link") => {
                let Some(href) = link_href(mark, ctx) else {
                    continue;
                };
                let _ = write!(out, "<a href=\"{}\"", esc(&href));
                if mark.pointer("/attrs/newTab") == Some(&Value::Bool(true)) {
                    out.push_str(" target=\"_blank\" rel=\"noopener noreferrer\"");
                }
                out.push('>');
                close.push("a");
                continue;
            }
            _ => continue,
        };
        let _ = write!(out, "<{tag}>");
        close.push(tag);
    }
    out.push_str(&esc(node.get("text").and_then(Value::as_str).unwrap_or("")));
    for tag in close.into_iter().rev() {
        let _ = write!(out, "</{tag}>");
    }
}

/// Where a link mark goes, if it may go anywhere: its `href`, or its document's URL, and
/// either one only if it passes the scheme check (W1-R21).
fn link_href(mark: &Value, ctx: &RenderCtx) -> Option<String> {
    let attrs = mark.get("attrs")?;
    let href = match (attrs.get("href"), attrs.get("doc")) {
        (Some(Value::String(href)), _) => href.clone(),
        (_, Some(doc)) => {
            let to = doc.get("relationTo")?.as_str()?;
            (ctx.link_url)(to, doc.get("value")?)?
        }
        _ => return None,
    };
    safe_href(&href).then_some(href)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{A1, P1, P2, doc, full, linked, para, text};
    use super::*;
    use serde_json::json;

    /// Post `P1` lives at `/blog/hello`; post `P2`'s URL is hostile. Author `A1` is
    /// "Ann <b>". Nothing else is readable.
    fn ctx_for(test: impl FnOnce(&RenderCtx)) {
        let link_url = |to: &str, v: &Value| match (to, v.as_str()?) {
            ("posts", P1) => Some("/blog/hello".to_owned()),
            ("posts", P2) => Some("JavaScript:alert(1)".to_owned()),
            _ => None,
        };
        let title = |to: &str, v: &Value| {
            (to == "authors" && v.as_str()? == A1).then(|| "Ann <b>".to_owned())
        };
        let slug = |_: &str, _: &Value| None;
        test(&RenderCtx {
            link_url: &link_url,
            title: &title,
            slug: &slug,
        })
    }

    const TAGS: &[&str] = &[
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "li",
        "blockquote",
        "hr",
        "br",
        "pre",
        "code",
        "table",
        "tr",
        "td",
        "th",
        "div",
        "strong",
        "em",
        "u",
        "s",
        "sub",
        "sup",
        "a",
    ];

    /// Fails unless `html` is inert: no `<script` or `javascript:` anywhere; only the tags
    /// the renderer writes; each attribute a plain name (never `on…`), bare or with a
    /// double-quoted value holding no `<`; and no raw `<` or `>` in text.
    fn assert_inert(html: &str) {
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("<script"), "{html}");
        assert!(!lower.contains("javascript:"), "{html}");
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            assert!(!rest[..open].contains('>'), "a raw > in text: {html}");
            let close = open + rest[open..].find('>').expect("an unclosed tag");
            let tag = &rest[open + 1..close];
            let (name, mut attrs) = tag.split_once(' ').unwrap_or((tag, ""));
            let name = name.strip_prefix('/').unwrap_or(name);
            assert!(TAGS.contains(&name), "tag `{name}` in {html}");
            while !attrs.is_empty() {
                let end = attrs.find(['=', ' ']).unwrap_or(attrs.len());
                let key = &attrs[..end];
                assert!(
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                        && !key.starts_with("on"),
                    "attribute `{key}` in {html}"
                );
                attrs = &attrs[end..];
                if let Some(value) = attrs.strip_prefix("=\"") {
                    let quote = value.find('"').expect("an unclosed attribute");
                    assert!(
                        !value[..quote].contains('<'),
                        "a raw < in an attribute: {html}"
                    );
                    attrs = &value[quote + 1..];
                }
                attrs = attrs.strip_prefix(' ').unwrap_or(attrs);
            }
            rest = &rest[close + 1..];
        }
        assert!(!rest.contains('>'), "a raw > in text: {html}");
    }

    #[test]
    fn html_output_is_inert() {
        let evil = "\"><img src=x onerror=alert(1)><script>alert(1)</script>";
        let corpus = [
            doc(json!([para(json!([text(evil)]))])),
            doc(json!([{"type": "heading", "attrs": {"level": 1}, "content": [text(evil)]}])),
            // A language that would break out of the class attribute, as stored before a
            // rule, and code that tries to close its element.
            doc(
                json!([{"type": "codeBlock", "attrs": {"language": "x\" onmouseover=\"alert(1)"},
                "content": [text("</code></pre><script>alert(1)</script>")]}]),
            ),
            doc(json!([para(json!([
                linked(json!({"href": "JaVaScRiPt:alert(1)"})),
                linked(json!({"href": "java\tscript:alert(1)"})),
                linked(json!({"href": " javascript:alert(1)"})),
                linked(json!({"href": "&#106;avascript:alert(1)"})),
                linked(json!({"href": "javascript&colon;alert(1)"})),
                linked(json!({"href": "//evil.example"})),
                linked(json!({"href": "data:text/html,<script>alert(1)</script>"})),
                linked(json!({"href": "vbscript:msgbox(1)"})),
                linked(json!({"href": "https://ok.example/?q=\"><script>alert(1)</script>"})),
                linked(json!({"href": ["javascript:alert(1)"]})),
                linked(json!({"doc": {"relationTo": "posts", "value": P2}, "newTab": true})),
                {"type": "text", "text": evil, "marks": [{"type": "link", "attrs": {"href": "/a", "onclick": "x"}}]}
            ]))])),
            doc(json!([{"type": "relationship", "attrs": {"relationTo": "authors", "value": A1}}])),
            doc(
                json!([{"type": "block", "attrs": {"blockType": "x\" onclick=\"alert(1)", "fields": {
                    "label": evil, "onclick": "alert(1)", "count": 3, "on": true,
                    "nested": {"html": "<script>alert(1)</script>"}, "bad name\" onclick": "y"
                }}}]),
            ),
            doc(
                json!([{"type": "orderedList", "attrs": {"start": "1\" onclick=\"x"},
                "content": [{"type": "listItem", "content": [para(json!([text(evil)]))]}]}]),
            ),
            doc(json!([{"type": "script", "content": [text("alert(1)")]}])),
            full(),
        ];
        ctx_for(|ctx| {
            for d in &corpus {
                assert_inert(&to_html(d, ctx));
            }
            // Refused hrefs leave the text alone, without a link.
            let links = to_html(&corpus[3], ctx);
            assert_eq!(links.matches("<a ").count(), 2, "{links}");
            assert!(links.contains("<a href=\"https://ok.example/?q=&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\">x</a>"), "{links}");
            assert!(links.contains("<a href=\"/a\">&quot;&gt;"), "{links}");
            // The checker itself catches what it's there to catch.
            for bad in [
                "<p onclick=\"x\">a</p>",
                "<p>a<b</p>",
                "<a href=\"javascript:x\">a</a>",
                "<img src=x>",
                "<div data-x=\"<\"></div>",
                "<p>a>b</p>",
            ] {
                assert!(
                    std::panic::catch_unwind(|| assert_inert(bad)).is_err(),
                    "{bad}"
                );
            }
        });
    }

    #[test]
    fn html_renders_every_node() {
        ctx_for(|ctx| {
            assert_eq!(
                to_html(&full(), ctx),
                concat!(
                    "<h2 id=\"title\">Title</h2>",
                    "<p><strong>b</strong><em><u><s>i</s></u></em><sub>s</sub><sup><code>S</code></sup><br>",
                    "<a href=\"https://example.com/a?b=c#d\">x</a>",
                    "<a href=\"http://example.com\">x</a>",
                    "<a href=\"mailto:ann@example.com\">x</a>",
                    "<a href=\"tel:+441234567890\">x</a>",
                    "<a href=\"/about\">x</a>",
                    "<a href=\"#top\">x</a>",
                    "<a href=\"/blog/hello\">x</a>",
                    "<a href=\"/new\" target=\"_blank\" rel=\"noopener noreferrer\">x</a>",
                    "<a href=\"/blog/hello\">x</a>",
                    "<a href=\"/same\">x</a></p>",
                    "<p></p>",
                    "<ul><li><p>one</p><ol start=\"3\"><li><p>three</p></li></ol></li></ul>",
                    "<ol><li><p></p></li></ol>",
                    "<ul data-task-list><li data-checked=\"true\"><p>done</p></li>",
                    "<li data-checked=\"false\"><p>to do</p></li></ul>",
                    "<blockquote><p>quoted</p></blockquote>",
                    "<hr>",
                    "<pre><code class=\"language-c++\">int main() {}</code></pre>",
                    "<pre><code></code></pre>",
                    "<table><tr><th colspan=\"2\"><p>c</p></th></tr>",
                    "<tr><td><p>c</p></td><td rowspan=\"2\"><p>c</p></td></tr>",
                    "<tr><td><p>c</p></td></tr></table>",
                    "<div data-relationship>Ann &lt;b&gt;</div>",
                    "<div data-block=\"cta\" data-author=\"0199d5c4-0000-7000-8000-0000000000b2\" ",
                    "data-label=\"Go\" data-meta=\"{&quot;editor&quot;:&quot;0199d5c4-0000-7000-8000-0000000000b1&quot;}\"></div>",
                    "<div data-block=\"cta\" data-label=\"New\"></div>",
                )
            );
            // Unique heading anchors; a camelCase field is a dashed data attribute.
            let d = doc(json!([
                {"type": "heading", "attrs": {"level": 1}, "content": [text("Intro")]},
                {"type": "heading", "attrs": {"level": 7}, "content": [text("Intro")]},
                {"type": "block", "attrs": {"blockType": "cta", "fields": {"ctaLabel": "Go", "gone": null}}}
            ]));
            assert_eq!(
                to_html(&d, ctx),
                "<h1 id=\"intro\">Intro</h1><h6 id=\"intro-2\">Intro</h6><div data-block=\"cta\" data-cta-label=\"Go\"></div>"
            );
        });
    }
}
