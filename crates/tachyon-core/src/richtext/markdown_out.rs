//! Rich text as Markdown: GitHub-flavoured, with the extensions Markdown input reads
//! (`:::block{…}` directives, `::ref{…}`, `doc:` links and `<u>`).

use serde_json::Value;

use super::{RenderCtx, attr, children, code_language, field_text, kind, safe_href};

/// `doc` as Markdown. Text is escaped so it reads back as text. What Markdown can't say is
/// left out: subscript and superscript are plain text, table cells don't span, and a
/// link's `newTab` is dropped. An internal link the context has no slug for, or an `href`
/// that fails the scheme check, is its text alone; so is a relationship node, which is
/// then left out.
// ponytail: each text node wraps its own marks, so two touching nodes with different marks
// can give delimiter runs (`**b***i*`) that read back differently, and only a paragraph's
// first line is escaped against starting a heading or list. Merge mark runs across nodes
// if Markdown input's round trip shows it matters.
pub fn to_markdown(doc: &Value, ctx: &RenderCtx) -> String {
    blocks(children(doc), ctx)
}

/// Block nodes apart by a blank line, empty ones left out.
fn blocks(nodes: &[Value], ctx: &RenderCtx) -> String {
    let parts: Vec<String> = nodes
        .iter()
        .map(|n| block(n, ctx))
        .filter(|b| !b.is_empty())
        .collect();
    parts.join("\n\n")
}

fn block(node: &Value, ctx: &RenderCtx) -> String {
    let number = |name| attr(node, name).and_then(Value::as_u64);
    match kind(node) {
        "paragraph" => line_start(inline(children(node), ctx)),
        "heading" => {
            let level = number("level").unwrap_or(1).clamp(1, 6) as usize;
            format!("{} {}", "#".repeat(level), inline(children(node), ctx))
        }
        "bulletList" => list(node, ctx, |_, _| "- ".into()),
        "orderedList" => {
            let start = number("start").unwrap_or(1);
            list(node, ctx, |i, _| format!("{}. ", start + i as u64))
        }
        "taskList" => list(node, ctx, |_, item| {
            match attr(item, "checked") == Some(&Value::Bool(true)) {
                true => "- [x] ".into(),
                false => "- [ ] ".into(),
            }
        }),
        "blockquote" => blocks(children(node), ctx)
            .lines()
            .map(|l| {
                if l.is_empty() {
                    ">".into()
                } else {
                    format!("> {l}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "horizontalRule" => "---".into(),
        "codeBlock" => {
            let code: String = children(node)
                .iter()
                .filter_map(|t| t.get("text")?.as_str())
                .collect();
            let fence = "`".repeat(longest_run(&code, '`').max(2) + 1);
            let lang = code_language(node).unwrap_or("");
            match code.is_empty() {
                true => format!("{fence}{lang}\n{fence}"),
                false => format!("{fence}{lang}\n{code}\n{fence}"),
            }
        }
        "table" => {
            let mut lines = Vec::new();
            for (i, row) in children(node).iter().enumerate() {
                let cells: Vec<String> = children(row)
                    .iter()
                    .map(|cell| blocks(children(cell), ctx).replace('\n', " "))
                    .collect();
                lines.push(format!("| {} |", cells.join(" | ")));
                if i == 0 {
                    lines.push(format!("|{}", " --- |".repeat(cells.len())));
                }
            }
            lines.join("\n")
        }
        "relationship" => {
            let to = attr(node, "relationTo").and_then(Value::as_str);
            match to
                .zip(attr(node, "value"))
                .and_then(|(to, v)| Some((to, (ctx.slug)(to, v)?)))
            {
                Some((to, slug)) => format!(
                    "::ref{{collection=\"{}\" slug=\"{}\"}}",
                    quoted(to),
                    quoted(&slug)
                ),
                None => String::new(),
            }
        }
        "block" => directive(node, ctx),
        _ => String::new(),
    }
}

/// A list: each item's marker from `marker(index, item)`, its later lines indented to match.
fn list(node: &Value, ctx: &RenderCtx, marker: impl Fn(usize, &Value) -> String) -> String {
    let mut out = Vec::new();
    for (i, item) in children(node).iter().enumerate() {
        let marker = marker(i, item);
        // A nested list follows its paragraph directly; anything else needs a blank line,
        // or it would run on into the line before.
        let mut body = String::new();
        for child in children(item) {
            let text = block(child, ctx);
            if text.is_empty() {
                continue;
            }
            if !body.is_empty() {
                body.push_str(if kind(child).ends_with("List") {
                    "\n"
                } else {
                    "\n\n"
                });
            }
            body.push_str(&text);
        }
        if body.is_empty() {
            out.push(marker.trim_end().to_owned());
            continue;
        }
        let indent = " ".repeat(marker.len());
        let lines: Vec<String> = body
            .lines()
            .enumerate()
            .map(|(n, l)| match (n, l.is_empty()) {
                (0, _) => format!("{marker}{l}"),
                (_, true) => String::new(),
                _ => format!("{indent}{l}"),
            })
            .collect();
        out.push(lines.join("\n"));
    }
    out.join("\n")
}

/// A block node: `::type{key="value" …}`, or, when exactly one of its fields is rich text,
/// `:::type{…}` around that text, with enough colons to hold any directive inside.
fn directive(node: &Value, ctx: &RenderCtx) -> String {
    let block_type = attr(node, "blockType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fields = attr(node, "fields").and_then(Value::as_object);
    let fields: Vec<(&String, &Value)> = fields.into_iter().flatten().collect();
    let texts: Vec<&Value> = fields
        .iter()
        .map(|(_, v)| *v)
        .filter(|v| kind(v) == "doc")
        .collect();
    let body = match texts[..] {
        [text] => Some(to_markdown(text, ctx)),
        _ => None,
    };
    let attrs: Vec<String> = fields
        .iter()
        .filter(|(_, v)| body.is_none() || kind(v) != "doc")
        .filter(|(name, _)| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .filter_map(|(name, v)| Some(format!("{name}=\"{}\"", quoted(&field_text(v)?))))
        .collect();
    let head = match attrs.is_empty() {
        true => block_type.to_owned(),
        false => format!("{block_type}{{{}}}", attrs.join(" ")),
    };
    let Some(body) = body else {
        return format!("::{head}");
    };
    let colons = ":".repeat(
        body.lines()
            .map(|l| l.len() - l.trim_start_matches(':').len())
            .max()
            .unwrap_or(0)
            .max(2)
            + 1,
    );
    match body.is_empty() {
        true => format!("{colons}{head}\n{colons}"),
        false => format!("{colons}{head}\n{body}\n{colons}"),
    }
}

/// `s` inside a directive's double-quoted attribute.
fn quoted(s: &str) -> String {
    // A newline would end the directive's line; Markdown input reads `\n` back (W1-R25).
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// The longest run of `c` in `s`.
fn longest_run(s: &str, c: char) -> usize {
    s.split(|x| x != c).map(str::len).max().unwrap_or(0)
}

/// `s` with what would make its first line a heading, list, rule or directive escaped.
fn line_start(s: String) -> String {
    if s.starts_with(['#', '-', '+', '=', ':']) {
        return format!("\\{s}");
    }
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 && s[digits..].starts_with(['.', ')']) {
        return format!("{}\\{}", &s[..digits], &s[digits..]);
    }
    s
}

/// Text that reads back as itself, not as Markdown.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        // `!` too, or text ending in one would turn a link after it into an image.
        if "\\`*_[]<>~|&!".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A link destination: brackets and backslashes escaped.
fn destination(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\()".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn inline(nodes: &[Value], ctx: &RenderCtx) -> String {
    let mut out = String::new();
    for node in nodes {
        match kind(node) {
            "hardBreak" => out.push_str("\\\n"),
            "text" => text(node, ctx, &mut out),
            _ => {}
        }
    }
    out
}

/// A text node inside its marks, the first outermost. Spaces at either end go outside the
/// marks, where Markdown needs them.
fn text(node: &Value, ctx: &RenderCtx, out: &mut String) {
    let raw = node.get("text").and_then(Value::as_str).unwrap_or("");
    let marks = node
        .get("marks")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let is = |t: &str| {
        marks
            .iter()
            .any(|m| m.get("type").and_then(Value::as_str) == Some(t))
    };
    let core = raw.trim();
    let (lead, trail) = (
        &raw[..raw.len() - raw.trim_start().len()],
        &raw[raw.trim_end().len()..],
    );
    if core.is_empty() {
        out.push_str(&escape(raw));
        return;
    }
    let mut inner = match is("code") {
        true => {
            let fence = "`".repeat(longest_run(core, '`') + 1);
            let pad = if core.starts_with('`') || core.ends_with('`') {
                " "
            } else {
                ""
            };
            format!("{fence}{pad}{core}{pad}{fence}")
        }
        false => escape(core),
    };
    for mark in marks.iter().rev() {
        let (open, close) = match mark.get("type").and_then(Value::as_str) {
            Some("bold") => ("**", "**".into()),
            Some("italic") => ("*", "*".into()),
            Some("strike") => ("~~", "~~".into()),
            Some("underline") => ("<u>", "</u>".into()),
            Some("link") => match link_destination(mark, ctx) {
                Some(dest) => ("[", format!("]({})", destination(&dest))),
                None => continue,
            },
            _ => continue,
        };
        inner = format!("{open}{inner}{close}");
    }
    out.push_str(lead);
    out.push_str(&inner);
    out.push_str(trail);
}

/// A link mark's `href` if it passes the scheme check, or `doc:<collection>/<slug>`.
fn link_destination(mark: &Value, ctx: &RenderCtx) -> Option<String> {
    let attrs = mark.get("attrs")?;
    match (attrs.get("href"), attrs.get("doc")) {
        (Some(Value::String(href)), _) => safe_href(href).then(|| href.clone()),
        (_, Some(doc)) => {
            let to = doc.get("relationTo")?.as_str()?;
            Some(format!("doc:{to}/{}", (ctx.slug)(to, doc.get("value")?)?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{A1, P1, doc, full, para, text};
    use super::*;
    use serde_json::json;

    /// Post `P1`'s slug is `hello` and author `A1`'s is `ann`; nothing else is readable.
    fn ctx_for(test: impl FnOnce(&RenderCtx)) {
        let none = |_: &str, _: &Value| None;
        let slug = |to: &str, v: &Value| match (to, v.as_str()?) {
            ("posts", P1) => Some("hello".to_owned()),
            ("authors", A1) => Some("ann".to_owned()),
            _ => None,
        };
        test(&RenderCtx {
            link_url: &none,
            title: &none,
            slug: &slug,
        })
    }

    #[test]
    fn markdown_renders_every_node() {
        ctx_for(|ctx| {
            assert_eq!(
                to_markdown(&full(), ctx),
                concat!(
                    "## Title\n\n",
                    "**b***<u>~~i~~</u>*s`S`\\\n",
                    "[x](https://example.com/a?b=c#d)[x](http://example.com)[x](mailto:ann@example.com)",
                    "[x](tel:+441234567890)[x](/about)[x](#top)[x](doc:posts/hello)[x](/new)",
                    "[x](doc:posts/hello)[x](/same)\n\n",
                    "- one\n",
                    "  3. three\n\n",
                    "1.\n\n",
                    "- [x] done\n",
                    "- [ ] to do\n\n",
                    "> quoted\n\n",
                    "---\n\n",
                    "```c++\nint main() {}\n```\n\n",
                    "```\n```\n\n",
                    "| c |\n| --- |\n| c | c |\n| c |\n\n",
                    "::ref{collection=\"authors\" slug=\"ann\"}\n\n",
                    "::cta{author=\"0199d5c4-0000-7000-8000-0000000000b2\" label=\"Go\" ",
                    "meta=\"{\\\"editor\\\":\\\"0199d5c4-0000-7000-8000-0000000000b1\\\"}\"}\n\n",
                    "::cta{label=\"New\"}"
                )
            );
        });
    }

    #[test]
    fn markdown_escapes_and_nests() {
        ctx_for(|ctx| {
            let callout = |body: Value| {
                json!({"type": "block", "attrs": {"blockType": "callout",
                "fields": {"kind": "warn \"x\"", "body": body}}})
            };
            let d = doc(json!([
                para(json!([text("# not *a* [heading] <b> | & \\")])),
                para(json!([text("1. not a list")])),
                para(json!([text(":::not a directive")])),
                {"type": "codeBlock", "content": [text("a\n```\nb")]},
                para(json!([
                    {"type": "text", "text": "a`b", "marks": [{"type": "code"}]},
                    {"type": "text", "text": " spaced ", "marks": [{"type": "bold"}]},
                    {"type": "text", "text": "hidden", "marks": [{"type": "link", "attrs": {"doc": {"relationTo": "posts", "value": "gone"}}}]},
                    {"type": "text", "text": "bad", "marks": [{"type": "link", "attrs": {"href": "javascript:alert(1)"}}]},
                    {"type": "text", "text": "(p)", "marks": [{"type": "link", "attrs": {"href": "/a_(b)"}}]}
                ])),
                para(json!([
                    text("Wow!"),
                    {"type": "text", "text": "x", "marks": [{"type": "link", "attrs": {"href": "/a"}}]}
                ])),
                {"type": "block", "attrs": {"blockType": "note", "fields": {"lines": "one\ntwo\\n"}}},
                {"type": "relationship", "attrs": {"relationTo": "authors", "value": "gone"}},
                callout(doc(json!([para(json!([text("inside")])), callout(doc(json!([para(json!([text("deeper")]))])))])))
            ]));
            assert_eq!(
                to_markdown(&d, ctx),
                concat!(
                    "\\# not \\*a\\* \\[heading\\] \\<b\\> \\| \\& \\\\\n\n",
                    "1\\. not a list\n\n",
                    "\\:::not a directive\n\n",
                    "````\na\n```\nb\n````\n\n",
                    "``a`b`` **spaced** hiddenbad[(p)](/a_\\(b\\))\n\n",
                    "Wow\\![x](/a)\n\n",
                    "::note{lines=\"one\\ntwo\\\\n\"}\n\n",
                    "::::callout{kind=\"warn \\\"x\\\"\"}\n",
                    "inside\n\n",
                    ":::callout{kind=\"warn \\\"x\\\"\"}\n",
                    "deeper\n",
                    ":::\n",
                    "::::"
                )
            );
        });
    }
}
