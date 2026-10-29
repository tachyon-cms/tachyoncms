//! Rich text as Markdown: GitHub-flavoured, with the extensions Markdown input reads
//! (`:::block{…}` directives, `::ref{…}`, `doc:` links and `<u>`).

use serde_json::Value;

use super::{RenderCtx, attr, children, code_language, field_text, kind, safe_href};
use crate::schema::FieldKind;

/// `doc` as Markdown. Text is escaped so it reads back as text. What Markdown can't say is
/// left out: subscript and superscript are plain text, table cells don't span, and a
/// link's `newTab` is dropped. An internal link the context has no slug for, or an `href`
/// that fails the scheme check, is its text alone; so is a relationship node, which is
/// then left out. `kind` is the rich text field's; it says how block attributes are
/// written.
// ponytail: each text node wraps its own marks, so two touching nodes with different marks
// can give delimiter runs (`**b***i*`) that read back differently. Merge mark runs across
// nodes if Markdown input's round trip shows it matters.
pub fn to_markdown(doc: &Value, kind: &FieldKind, ctx: &RenderCtx) -> String {
    blocks(children(doc), Some(kind), ctx)
}

/// Block nodes apart by a blank line, empty ones left out.
fn blocks(nodes: &[Value], k: Option<&FieldKind>, ctx: &RenderCtx) -> String {
    let parts: Vec<String> = nodes
        .iter()
        .map(|n| block(n, k, ctx))
        .filter(|b| !b.is_empty())
        .collect();
    parts.join("\n\n")
}

fn block(node: &Value, k: Option<&FieldKind>, ctx: &RenderCtx) -> String {
    let number = |name| attr(node, name).and_then(Value::as_u64);
    match kind(node) {
        "paragraph" => line_start(inline(children(node), ctx)),
        "heading" => {
            let level = number("level").unwrap_or(1).clamp(1, 6) as usize;
            format!(
                "{} {}",
                "#".repeat(level),
                line_start(inline(children(node), ctx))
            )
        }
        "bulletList" => list(node, k, ctx, |_, _| "- ".into()),
        "orderedList" => {
            let start = number("start").unwrap_or(1);
            list(node, k, ctx, |i, _| format!("{}. ", start + i as u64))
        }
        "taskList" => list(node, k, ctx, |_, item| {
            match attr(item, "checked") == Some(&Value::Bool(true)) {
                true => "- [x] ".into(),
                false => "- [ ] ".into(),
            }
        }),
        "blockquote" => blocks(children(node), k, ctx)
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
                    .map(|cell| blocks(children(cell), k, ctx).replace('\n', " "))
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
        "block" => directive(node, k, ctx),
        _ => String::new(),
    }
}

/// A list: each item's marker from `marker(index, item)`, its later lines indented to match.
fn list(
    node: &Value,
    k: Option<&FieldKind>,
    ctx: &RenderCtx,
    marker: impl Fn(usize, &Value) -> String,
) -> String {
    let mut out = Vec::new();
    for (i, item) in children(node).iter().enumerate() {
        let marker = marker(i, item);
        // A nested list follows its paragraph directly; anything else needs a blank line,
        // or it would run on into the line before. So does a numbered list starting past
        // 1, which Markdown won't let interrupt a paragraph.
        let mut body = String::new();
        for child in children(item) {
            let text = block(child, k, ctx);
            if text.is_empty() {
                continue;
            }
            let runs_on = kind(child).ends_with("List")
                && attr(child, "start")
                    .and_then(Value::as_u64)
                    .is_none_or(|s| s == 1);
            if !body.is_empty() {
                body.push_str(if runs_on { "\n" } else { "\n\n" });
            }
            body.push_str(&text);
        }
        if body.is_empty() {
            out.push(marker.trim_end().to_owned());
            continue;
        }
        // A task item's content starts after its `- `; its box is part of the content.
        let width = ["[ ] ", "[x] "]
            .iter()
            .find_map(|b| marker.strip_suffix(b))
            .unwrap_or(&marker)
            .len();
        let indent = " ".repeat(width);
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
fn directive(node: &Value, k: Option<&FieldKind>, ctx: &RenderCtx) -> String {
    let block_type = attr(node, "blockType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let def = k.and_then(|k| k.block(block_type));
    let field = |name: &str| def.and_then(|d| d.fields.iter().find(|f| f.name == name));
    let fields = attr(node, "fields").and_then(Value::as_object);
    let fields: Vec<(&String, &Value)> = fields.into_iter().flatten().collect();
    // Rich text by its field, or by its shape where the block type isn't known; a json
    // field holding a doc-shaped value is an attribute.
    let texts: Vec<&(&String, &Value)> = fields
        .iter()
        .filter(|(name, v)| {
            kind(v) == "doc"
                && field(name).is_none_or(|f| matches!(f.kind, FieldKind::RichText { .. }))
        })
        .collect();
    let (body_name, body) = match texts[..] {
        [(name, text)] => (
            Some(*name),
            Some(blocks(children(text), field(name).map(|f| &f.kind), ctx)),
        ),
        _ => (None, None),
    };
    let attrs: Vec<String> = fields
        .iter()
        .filter(|(name, _)| Some(*name) != body_name)
        .filter(|(name, _)| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .filter_map(|(name, v)| {
            // A json field's string is written as JSON too, so `"30"` doesn't read back as 30.
            let text = match field(name).map(|f| &f.kind) {
                Some(FieldKind::Json) => Some(v.to_string()),
                _ => field_text(v),
            };
            Some(format!("{name}=\"{}\"", quoted(&text?)))
        })
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
    // A line break would end the directive's line; Markdown input reads `\n` and `\r` back
    // (W1-R25).
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// The longest run of `c` in `s`.
fn longest_run(s: &str, c: char) -> usize {
    s.split(|x| x != c).map(str::len).max().unwrap_or(0)
}

/// `s` with what would make its first line a heading, list, rule or directive escaped.
fn line_start(s: String) -> String {
    // Indent is written as entities, which Markdown keeps as text, so what follows it can't
    // start structure (W1-R30).
    let indent = s.len() - s.trim_start_matches([' ', '\t']).len();
    if indent > 0 {
        let entities: String = s[..indent]
            .chars()
            .map(|c| if c == ' ' { "&#32;" } else { "&#9;" })
            .collect();
        return format!("{entities}{}", &s[indent..]);
    }
    if s.starts_with(['#', '-', '+', '=', ':']) {
        return format!("\\{s}");
    }
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 && s[digits..].starts_with(['.', ')']) {
        return format!("{}\\{}", &s[..digits], &s[digits..]);
    }
    s
}

/// Text that reads back as itself, not as Markdown. A line break is an entity, so it can't
/// start a line that reads as structure (W1-R28).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\r' => out.push_str(line_break(c)),
            // `!` too, or text ending in one would turn a link after it into an image.
            _ if "\\`*_[]<>~|&!".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `\n` or `\r` as an entity, which reads back as itself.
fn line_break(c: char) -> &'static str {
    if c == '\n' { "&#10;" } else { "&#13;" }
}

/// `s` in a code span. Code spans can't hold a line break, so the span ends before each
/// one and starts again after it, the break written outside as an entity (W1-R28).
fn code_spans(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        // Spaces at a piece's ends go outside it, where Markdown keeps them.
        let piece = &rest[..end];
        let span = piece.trim_matches(' ');
        let lead = &piece[..piece.len() - piece.trim_start_matches(' ').len()];
        out.push_str(lead);
        if !span.is_empty() {
            let fence = "`".repeat(longest_run(span, '`') + 1);
            let pad = if span.starts_with('`') || span.ends_with('`') {
                " "
            } else {
                ""
            };
            out.push_str(&format!("{fence}{pad}{span}{pad}{fence}"));
            out.push_str(&piece[lead.len() + span.len()..]);
        }
        let Some(c) = rest[end..].chars().next() else {
            return out;
        };
        out.push_str(line_break(c));
        rest = &rest[end + 1..];
    }
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

/// Inline nodes. Each line after a hard break is escaped as a paragraph's first line is.
fn inline(nodes: &[Value], ctx: &RenderCtx) -> String {
    let mut lines = vec![String::new()];
    // Whether the last text ended in a delimiter run with punctuation inside it.
    let mut closed_on_punct = false;
    for node in nodes {
        match kind(node) {
            "hardBreak" => {
                lines.push(String::new());
                closed_on_punct = false;
            }
            "text" => {
                let mut r = String::new();
                text(node, ctx, &mut r);
                let line = lines.last_mut().expect("never empty");
                // A `*` or `~` run with punctuation inside it only opens or closes with
                // whitespace or punctuation outside it, so a letter there is an entity.
                if delimited(&r, true)
                    && let Some(c) = line.chars().last().filter(|c| c.is_alphanumeric())
                {
                    line.pop();
                    line.push_str(&entity(c));
                }
                if closed_on_punct && let Some(c) = r.chars().next().filter(|c| c.is_alphanumeric())
                {
                    r = format!("{}{}", entity(c), &r[c.len_utf8()..]);
                }
                closed_on_punct = delimited(&r, false);
                line.push_str(&r);
            }
            _ => {}
        }
    }
    let first = line_ends(&lines.remove(0));
    lines.into_iter().fold(first, |out, l| {
        format!("{out}\\\n{}", line_start(line_ends(&l)))
    })
}

/// Whether `s` starts (or, with `start` false, ends) with a `*` or `~` delimiter run that
/// has punctuation or a symbol on its inner side.
fn delimited(s: &str, start: bool) -> bool {
    let is_run = |c: &char| *c == '*' || *c == '~';
    let (edge, inner) = match start {
        true => (s.chars().next(), s.chars().find(|c| !is_run(c))),
        false => (s.chars().next_back(), s.chars().rev().find(|c| !is_run(c))),
    };
    edge.is_some_and(|c| is_run(&c))
        && inner.is_some_and(|c| !c.is_alphanumeric() && !c.is_whitespace())
}

/// `c` as a numeric character reference.
fn entity(c: char) -> String {
    format!("&#{};", c as u32)
}

/// `s` with the spaces and tabs before its end, and before each line break written as an
/// entity, written as entities too: Markdown drops them there, or reads two spaces as a
/// hard break (W1-R31).
fn line_ends(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == ' ' || c == '\t' {
            run.push(c);
        } else {
            let at_end = rest.starts_with("&#10;") || rest.starts_with("&#13;");
            flush(&mut out, &mut run, at_end);
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut out, &mut run, true);
    out
}

/// Ends a run of spaces and tabs: as entities where a line ends, as they are elsewhere.
fn flush(out: &mut String, run: &mut String, at_end: bool) {
    match at_end {
        true => out.extend(run.chars().map(|c| if c == ' ' { "&#32;" } else { "&#9;" })),
        false => out.push_str(run),
    }
    run.clear();
}

/// A text node inside its marks, the first outermost.
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
    // Edge whitespace goes inside the marks as entities, which keep the delimiters next to
    // text Markdown accepts them beside (W1-R32). A code span would show entities as
    // written, so its edges go outside it instead.
    let inside = !marks.is_empty() && !is("code");
    let (lead, trail) = match inside {
        true => ("", ""),
        false => (lead, trail),
    };
    let mut inner = match (is("code"), inside) {
        (true, _) if core.is_empty() => return out.push_str(&escape(raw)),
        (true, _) => code_spans(core),
        (false, true) => {
            let edge = |s: &str| -> String { s.chars().map(entity).collect() };
            let core = raw.trim();
            let lead = &raw[..raw.len() - raw.trim_start().len()];
            format!(
                "{}{}{}",
                edge(lead),
                escape(core),
                edge(&raw[lead.len() + core.len()..])
            )
        }
        (false, false) if core.is_empty() => return out.push_str(&escape(raw)),
        (false, false) => escape(core),
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
    out.push_str(&escape(lead));
    out.push_str(&inner);
    out.push_str(&escape(trail));
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

    /// A field that defines no block types, so block attributes are written by value.
    fn plain() -> FieldKind {
        let f: crate::schema::Field =
            serde_json::from_value(json!({"name": "body", "type": "richText"})).unwrap();
        f.kind
    }

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
                to_markdown(&full(), &plain(), ctx),
                concat!(
                    "## Title\n\n",
                    "**b***<u>~~i~~</u>*&#115;`S`\\\n",
                    "[x](https://example.com/a?b=c#d)[x](http://example.com)[x](mailto:ann@example.com)",
                    "[x](tel:+441234567890)[x](/about)[x](#top)[x](doc:posts/hello)[x](/new)",
                    "[x](doc:posts/hello)[x](/same)\n\n",
                    "- one\n\n",
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
                to_markdown(&d, &plain(), ctx),
                concat!(
                    "\\# not \\*a\\* \\[heading\\] \\<b\\> \\| \\& \\\\\n\n",
                    "1\\. not a list\n\n",
                    "\\:::not a directive\n\n",
                    "````\na\n```\nb\n````\n\n",
                    "``a`b``**&#32;spaced&#32;**&#104;iddenbad[(p)](/a_\\(b\\))\n\n",
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
