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
/// A table cell's blocks become one paragraph, hard breaks between them (W1-R37).
/// A text's marks read back in one order, since Markdown can't say theirs (W1-R37).
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
        "paragraph" => line_start(inline(children(node), ctx, Where::Paragraph)),
        "heading" => {
            let level = number("level").unwrap_or(1).clamp(1, 6) as usize;
            let text = line_start(inline(children(node), ctx, Where::Heading));
            // A trailing run of `#` after a space would read as the heading's closing one.
            let bare = text.trim_end_matches('#');
            let text = match bare.len() < text.len() && (bare.is_empty() || bare.ends_with(' ')) {
                true => format!("{bare}\\{}", &text[bare.len()..]),
                false => text,
            };
            format!("{} {text}", "#".repeat(level))
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
                    .map(|cell| line_start(inline(&cell_inline(cell), ctx, Where::Cell)))
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
    let indent = s.len() - s.trim_start_matches([' ', '\t', '\u{b}', '\u{c}']).len();
    if indent > 0 {
        let entities: String = s[..indent].chars().map(entity).collect();
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

/// `s` in code spans. A span can't hold a line break, so it ends before each one and starts
/// again after it, the break written outside as an entity (W1-R28). Spaces stay inside: a
/// span with a space at both ends is padded with one more, which Markdown strips.
fn code_spans(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let span = &rest[..end];
        if !span.is_empty() {
            let fence = "`".repeat(longest_run(span, '`') + 1);
            let spaced =
                span.starts_with(' ') && span.ends_with(' ') && !span.trim_matches(' ').is_empty();
            let pad = match span.starts_with('`') || span.ends_with('`') || spaced {
                true => " ",
                false => "",
            };
            out.push_str(&format!("{fence}{pad}{span}{pad}{fence}"));
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

/// Where inline text sits, which decides how a hard break is written.
#[derive(Clone, Copy, PartialEq)]
enum Where {
    Paragraph,
    Heading,
    Cell,
}

/// Inline nodes. A hard break is `\` ending the line, and the line after it is escaped as a
/// paragraph's first line is. Where that can't be written (a heading, a table cell, the
/// end of a paragraph) it's `<br>` (W1-R36).
fn inline(nodes: &[Value], ctx: &RenderCtx, at: Where) -> String {
    let lines: Vec<String> = nodes
        .split(|n| kind(n) == "hardBreak")
        .map(|line| line_ends(&marked_line(line, ctx, at == Where::Cell)))
        .collect();
    let mut lines = lines.into_iter();
    let first = lines.next().unwrap_or_default();
    lines.fold(first, |out, l| {
        match at == Where::Paragraph && !l.is_empty() {
            true => format!("{out}\\\n{}", line_start(l)),
            false => format!("{out}<br>{l}"),
        }
    })
}

/// A table cell's blocks as one line of inline nodes, hard breaks between them: a cell is
/// one line of Markdown (W1-R37).
fn cell_inline(cell: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, block) in children(cell).iter().enumerate() {
        if i > 0 {
            out.push(serde_json::json!({"type": "hardBreak"}));
        }
        match kind(block) {
            "paragraph" | "heading" => out.extend(children(block).iter().cloned()),
            _ => {
                let doc = serde_json::json!({"type": "doc", "content": [block]});
                out.push(serde_json::json!({"type": "text", "text": super::to_text(&doc)}));
            }
        }
    }
    out
}

/// A piece of a line: text, or a delimiter run of `*`, `_` or `~` opening or closing the
/// mark that `pair` numbers.
enum Piece {
    Text(String),
    Run {
        ch: char,
        len: usize,
        open: bool,
        pair: usize,
    },
}

/// One line of text nodes, with their marks. A mark the next node shares stays open across
/// both, the longest-running outermost, so marks nest and read back as written (W1-R33).
fn marked_line(nodes: &[Value], ctx: &RenderCtx, cell: bool) -> String {
    // Adjacent text under the same marks is one text.
    let mut texts: Vec<(String, Vec<Value>, bool)> = Vec::new();
    for node in nodes.iter().filter(|n| kind(n) == "text") {
        let raw = node.get("text").and_then(Value::as_str).unwrap_or("");
        let all = node.get("marks").and_then(Value::as_array);
        let is_code = all.is_some_and(|m| m.iter().any(|m| m["type"] == "code"));
        // The marks Markdown can write, code apart: it's always innermost.
        let marks: Vec<Value> = all
            .into_iter()
            .flatten()
            .filter(|m| match m["type"].as_str() {
                Some("bold" | "italic" | "strike" | "underline") => true,
                Some("link") => link_destination(m, ctx).is_some(),
                _ => false,
            })
            .cloned()
            .collect();
        match texts.last_mut() {
            Some((text, m, code)) if *m == marks && *code == is_code => text.push_str(raw),
            _ => texts.push((raw.to_owned(), marks, is_code)),
        }
    }
    let mut pieces: Vec<Piece> = Vec::new();
    let mut open: Vec<(Value, usize)> = Vec::new();
    let mut pairs = 0;
    // Where each link opened, by its pair.
    let mut starts: Vec<usize> = Vec::new();
    let close = |pieces: &mut Vec<Piece>, starts: &[usize], (mark, pair): (Value, usize)| {
        let piece = match mark["type"].as_str() {
            Some("underline") => Piece::Text("</u>".into()),
            Some("link") => {
                let dest = destination(&link_destination(&mark, ctx).unwrap_or_default());
                let dest = if cell { dest.replace('|', "\\|") } else { dest };
                // A line starting `[` whose code holds `]:` reads as a link reference
                // definition, unless something follows its destination on the line: an
                // empty title does.
                let defines = starts[pair] == 0
                    && pieces[1..]
                        .iter()
                        .any(|p| matches!(p, Piece::Text(s) if s.contains("]:")));
                match defines {
                    true => Piece::Text(format!("]({dest} \"\")")),
                    false => Piece::Text(format!("]({dest})")),
                }
            }
            t => delimiter(t, false, pair),
        };
        pieces.push(piece);
    };
    for (i, (text, marks, code)) in texts.iter().enumerate() {
        let kept = open.iter().take_while(|(m, _)| marks.contains(m)).count();
        while open.len() > kept {
            close(&mut pieces, &starts, open.pop().expect("counted"));
        }
        // How many texts from here on carry `m`: the longer, the further out it opens.
        let reach = |m: &Value| texts[i..].iter().take_while(|t| t.1.contains(m)).count();
        let mut new: Vec<&Value> = marks
            .iter()
            .filter(|m| !open.iter().any(|(o, _)| o == *m))
            .collect();
        new.sort_by_key(|m| std::cmp::Reverse(reach(m)));
        for m in new {
            starts.push(pieces.len());
            pieces.push(match m["type"].as_str() {
                Some("underline") => Piece::Text("<u>".into()),
                Some("link") => Piece::Text("[".into()),
                t => delimiter(t, true, pairs),
            });
            open.push((m.clone(), pairs));
            pairs += 1;
        }
        let edges = !marks.is_empty();
        pieces.push(Piece::Text(match code {
            true if cell => code_spans(text).replace('|', "\\|"),
            true => code_spans(text),
            false if edges => edged(text),
            false => escape(text),
        }));
    }
    while let Some(o) = open.pop() {
        close(&mut pieces, &starts, o);
    }
    choose_delimiters(&mut pieces, pairs);
    flank(&mut pieces);
    pieces
        .into_iter()
        .map(|p| match p {
            Piece::Text(s) => s,
            Piece::Run { ch, len, .. } => ch.to_string().repeat(len),
        })
        .collect()
}

/// The delimiter run of a bold, italic or strike mark; `*` until `choose_delimiters`.
fn delimiter(t: Option<&str>, open: bool, pair: usize) -> Piece {
    let (ch, len) = match t {
        Some("bold") => ('*', 2),
        Some("italic") => ('*', 1),
        _ => ('~', 2),
    };
    Piece::Run {
        ch,
        len,
        open,
        pair,
    }
}

/// Picks `*` or `_` for each bold and italic mark so that no run touches another of the
/// same character, which would read as one run.
fn choose_delimiters(pieces: &mut [Piece], pairs: usize) {
    let at = |pieces: &[Piece], pair: usize| -> Vec<usize> {
        let found = pieces
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p, Piece::Run { pair: q, ch: '*' | '_', .. } if *q == pair));
        found.map(|(i, _)| i).collect()
    };
    for pair in 0..pairs {
        let sites = at(pieces, pair);
        if sites.is_empty() {
            continue;
        }
        // Runs already chosen beside either end of this mark.
        let beside: Vec<char> = sites
            .iter()
            .flat_map(|&i| [i.checked_sub(1), Some(i + 1)])
            .flatten()
            .filter_map(|j| match pieces.get(j) {
                Some(Piece::Run { ch, pair: q, .. }) if *q < pair || *ch == '~' => Some(*ch),
                _ => None,
            })
            .collect();
        let ch = ['*', '_']
            .into_iter()
            .find(|c| !beside.contains(c))
            .unwrap_or('*');
        for i in sites {
            if let Piece::Run { ch: c, .. } = &mut pieces[i] {
                *c = ch;
            }
        }
    }
}

/// A run opens only beside what's outside it on its left, and closes only beside what's on
/// its right, as Markdown's flanking rules allow. Where that's a letter or digit and the
/// run couldn't open or close beside it, the letter is written as an entity, which reads
/// as punctuation there and as itself in the text.
fn flank(pieces: &mut [Piece]) {
    // Encoding only ever turns a character into ASCII punctuation, so this ends; it runs
    // until nothing changes, since one run's fix can change another's neighbour.
    while (0..pieces.len()).any(|i| flank_one(pieces, i)) {}
}

/// Fixes the first neighbour of run `i` that stops it opening or closing, if any. Outside
/// ASCII, a neighbour is always encoded, rather than classified (W1-R35).
fn flank_one(pieces: &mut [Piece], i: usize) -> bool {
    let Piece::Run { ch, open, .. } = pieces[i] else {
        return false;
    };
    // The outer neighbour is before an opening run and after a closing one.
    let (outer, inner) = (neighbour(pieces, i, open), neighbour(pieces, i, !open));
    let c = |n: Option<(Option<usize>, char)>| n.map(|(_, c)| c);
    let fix = if c(outer).is_some_and(|c| !c.is_ascii()) {
        outer
    } else if c(inner).is_some_and(|c| !c.is_ascii()) {
        inner
    } else if c(outer).is_some_and(|c| c.is_ascii_alphanumeric())
        && (ch == '_' || c(inner).is_some_and(|c| c.is_ascii_punctuation()))
    {
        // `_` never opens or closes inside a word; `*` and `~` don't beside punctuation.
        outer
    } else {
        None
    };
    let Some((Some(j), _)) = fix else {
        return false;
    };
    let Piece::Text(s) = &mut pieces[j] else {
        return false;
    };
    match j < i {
        true => {
            let c = s.pop().expect("a neighbour");
            s.push_str(&entity(c));
        }
        false => {
            let c = s.remove(0);
            s.insert_str(0, &entity(c));
        }
    }
    true
}

/// The character beside run `i`, before it or after it, with the text piece it's in (none
/// when it's another run).
fn neighbour(pieces: &[Piece], i: usize, before: bool) -> Option<(Option<usize>, char)> {
    let at = |j: usize| match &pieces[j] {
        Piece::Text(s) if before => s.chars().next_back().map(|c| (Some(j), c)),
        Piece::Text(s) => s.chars().next().map(|c| (Some(j), c)),
        Piece::Run { ch, .. } => Some((None, *ch)),
    };
    match before {
        true => (0..i).rev().find_map(at),
        false => (i + 1..pieces.len()).find_map(at),
    }
}

/// Marked text: spaces and tabs at its ends as entities, inside the delimiters, where
/// Markdown keeps them (W1-R32).
fn edged(raw: &str) -> String {
    let core = raw.trim();
    let lead = &raw[..raw.len() - raw.trim_start().len()];
    let trail = &raw[lead.len() + core.len()..];
    let edge = |s: &str| -> String { s.chars().map(entity).collect() };
    format!("{}{}{}", edge(lead), escape(core), edge(trail))
}

/// `c` as a numeric character reference.
fn entity(c: char) -> String {
    format!("&#{};", c as u32)
}

/// `s` with the spaces, tabs, VT and FF before its end, and before each line break written
/// as an entity, written as entities too: Markdown drops them there, or reads two spaces
/// as a hard break (W1-R31, W1-R35).
fn line_ends(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if matches!(c, ' ' | '\t' | '\u{b}' | '\u{c}') {
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

/// Ends a run of spaces, tabs, VT and FF: as entities where a line ends, as they are elsewhere.
fn flush(out: &mut String, run: &mut String, at_end: bool) {
    match at_end {
        true => out.extend(run.chars().map(entity)),
        false => out.push_str(run),
    }
    run.clear();
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
                    "**b**_<u>~~i~~</u>_&#115;`S`\\\n",
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
