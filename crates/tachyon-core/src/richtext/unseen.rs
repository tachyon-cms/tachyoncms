//! What a Markdown write puts back. Markdown leaves out what its reader can't read: a
//! relationship or upload node whose document they can't read, and the link on text that
//! goes to one. A writer who sends that Markdown back never saw those, so replacing the text
//! with what they sent would delete content they couldn't see (#325). Each one goes back
//! where it was instead, as far as the text they sent still has that place.
//!
//! A node's place is its nearest preceding sibling the writer saw, counted among the siblings
//! they saw: "after the third block they saw", or "first" with none before it. Counting
//! rather than matching content keeps the node behind that block whatever was edited inside
//! it. If blocks moved or went, the node keeps the count; past the end, it goes last. A node
//! inside something the text no longer has (a list item deleted, a quote now a paragraph)
//! goes right after that thing's place in the nearest container that holds blocks. One inside
//! a block node's text stays in that text, which is a field of its own: if the block node
//! isn't at its place with the same block type, the node goes with it, as everything inside
//! a removed block node does. A link goes back on its text, the run nearest where it was in
//! the paragraph at its place; if that text was rewritten, the link goes with it.

use serde_json::{Value, json};

use super::{BLOCK_NODES, RenderCtx, attr, children, is_link_mark, kind, markdown_out};
use crate::schema::{Field, FieldKind};

/// Whether the writer reads a link's target, by its collection and `value`.
pub type Reads<'a> = &'a dyn Fn(&str, &Value) -> bool;

/// Puts into `new`, the rich text a Markdown write sent for a field of `kind`, what `old`, the
/// text it replaces, held that the writer couldn't read (see the module). `ctx` is how the
/// writer's Markdown read showed what the text links to.
pub fn restore_unseen(
    old: &Value,
    new: &mut Value,
    kind: &FieldKind,
    ctx: &RenderCtx,
    reads: Reads,
) {
    // A document holds blocks, so nothing is left over for a container around it.
    container(old, new, kind, ctx, reads);
}

/// `text` without the nodes the writer can't read, at any depth: what a Markdown write that
/// puts them back (`restore_unseen`) doesn't replace.
pub fn without_unseen(text: &mut Value, reads: Reads) {
    if let Some(Value::Array(nodes)) = text.get_mut("content") {
        nodes.retain(|n| !unseen(n, reads));
        nodes.iter_mut().for_each(|n| without_unseen(n, reads));
    }
    if let Some(Value::Object(fields)) = text.pointer_mut("/attrs/fields") {
        fields
            .values_mut()
            .filter(|v| kind(v) == "doc")
            .for_each(|v| without_unseen(v, reads));
    }
}

/// A relationship or upload node whose document the writer can't read.
fn unseen(node: &Value, reads: Reads) -> bool {
    matches!(kind(node), "relationship" | "upload") && !read(node.get("attrs"), reads)
}

/// Whether `link`, `{relationTo, value}`, goes to a document the writer reads.
fn read(link: Option<&Value>, reads: Reads) -> bool {
    link.and_then(|l| Some(reads(l.get("relationTo")?.as_str()?, l.get("value")?)))
        .unwrap_or(false)
}

/// Whether Markdown writes `node`, a block: not an empty paragraph, nor a node it can't
/// name, which the writer didn't see.
fn shows(node: &Value, k: &FieldKind, ctx: &RenderCtx) -> bool {
    match kind(node) {
        "paragraph" | "relationship" | "upload" => {
            !markdown_out::block_markdown(node, k, ctx).is_empty()
        }
        t => BLOCK_NODES.contains(&t),
    }
}

/// Whether `new` is how Markdown read `old` back: the same type, block type and file.
fn same(old: &Value, new: &Value) -> bool {
    kind(old) == kind(new)
        && match kind(old) {
            "block" => attr(old, "blockType") == attr(new, "blockType"),
            "upload" => {
                attr(old, "relationTo") == attr(new, "relationTo")
                    && attr(old, "value") == attr(new, "value")
            }
            _ => true,
        }
}

/// Puts back into `new`, read back from `old`, what the writer couldn't see among `old`'s
/// children and inside them. Returns the nodes with no place in `new` because it holds no
/// blocks (a list, a table row), for the container around it to take.
fn container(
    old: &Value,
    new: &mut Value,
    k: &FieldKind,
    ctx: &RenderCtx,
    reads: Reads,
) -> Vec<Value> {
    let blocks = matches!(
        kind(old),
        "doc" | "listItem" | "taskItem" | "blockquote" | "tableCell" | "tableHeader"
    );
    // A cell is one line of Markdown, its blocks one paragraph.
    let cell = matches!(kind(old), "tableCell" | "tableHeader");
    // Each node to put back, with how many of the children the writer saw come before it.
    let mut put: Vec<(usize, Value)> = Vec::new();
    let mut orphans = Vec::new();
    let mut seen = 0;
    for child in children(old) {
        if unseen(child, reads) {
            put.push((seen, child.clone()));
            continue;
        }
        if blocks && !shows(child, k, ctx) {
            continue;
        }
        let twin = new
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|c| c.get_mut(seen))
            .filter(|t| same(child, t));
        let lost = match twin {
            Some(twin) if !cell => descend(child, twin, k, ctx, reads),
            _ => unseen_inside(child, reads),
        };
        seen += 1;
        match blocks {
            true => put.extend(lost.into_iter().map(|n| (seen, n))),
            false => orphans.extend(lost),
        }
    }
    if cell {
        // Its links go back over the whole line, its blocks apart as Markdown wrote them.
        let line: Vec<&Value> = children(old)
            .iter()
            .filter(|b| matches!(kind(b), "paragraph" | "heading"))
            .flat_map(|b| children(b).iter().chain([&*HARD_BREAK]))
            .collect();
        if let Some(first) = new
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|c| c.first_mut())
            .filter(|p| kind(p) == "paragraph")
        {
            links(&line, first, reads);
        }
    }
    if put.is_empty() {
        return orphans;
    }
    let Some(obj) = new.as_object_mut() else {
        return orphans;
    };
    let Value::Array(content) = obj.entry("content").or_insert_with(|| json!([])) else {
        return orphans;
    };
    // The n-th node put back goes after the n before it, whose places are at or before its own.
    for (n, (at, node)) in put.into_iter().enumerate() {
        content.insert((at + n).min(content.len()), node);
    }
    orphans
}

static HARD_BREAK: std::sync::LazyLock<Value> =
    std::sync::LazyLock::new(|| json!({"type": "hardBreak"}));

/// Puts back into `new`, Markdown's reading of `old`, what the writer couldn't see inside
/// `old`. Returns what needs a container holding blocks further up.
fn descend(
    old: &Value,
    new: &mut Value,
    k: &FieldKind,
    ctx: &RenderCtx,
    reads: Reads,
) -> Vec<Value> {
    match kind(old) {
        "paragraph" | "heading" => {
            links(&children(old).iter().collect::<Vec<_>>(), new, reads);
            Vec::new()
        }
        "block" | "upload" => {
            body(old, new, k, ctx, reads);
            Vec::new()
        }
        _ => container(old, new, k, ctx, reads),
    }
}

/// The nodes the writer can't read anywhere inside `node`, block nodes' own text aside.
fn unseen_inside(node: &Value, reads: Reads) -> Vec<Value> {
    let mut out = Vec::new();
    for child in children(node) {
        match unseen(child, reads) {
            true => out.push(child.clone()),
            false => out.extend(unseen_inside(child, reads)),
        }
    }
    out
}

/// Puts back into the rich text the block or upload node `new` holds what the writer
/// couldn't see of `old`'s: the one text Markdown writes inside its directive, if it has one.
fn body(old: &Value, new: &mut Value, k: &FieldKind, ctx: &RenderCtx, reads: Reads) {
    let defs: Option<&[Field]> = match kind(old) {
        "block" => attr(old, "blockType")
            .and_then(Value::as_str)
            .and_then(|t| k.block(t))
            .map(|b| &b.fields[..]),
        _ => attr(old, "relationTo")
            .and_then(Value::as_str)
            .and_then(|c| k.upload_node(c))
            .map(|n| &n.fields[..]),
    };
    let Some(fields) = attr(old, "fields").and_then(Value::as_object) else {
        return;
    };
    // The directive's text: its one field holding rich text.
    let texts: Vec<(&String, &FieldKind)> = fields
        .iter()
        .filter(|(_, v)| kind(v) == "doc")
        .filter_map(|(name, _)| {
            let f = defs?.iter().find(|f| &f.name == name)?;
            matches!(f.kind, FieldKind::RichText { .. }).then_some((name, &f.kind))
        })
        .collect();
    let [(name, inner)] = texts[..] else {
        return;
    };
    if let Some(twin) = new
        .pointer_mut(&format!("/attrs/fields/{name}"))
        .filter(|t| kind(t) == "doc")
    {
        container(&fields[name.as_str()], twin, inner, ctx, reads);
    }
}

/// The text a node adds to its line: a text node's, `\n` for a hard break.
fn line_of(n: &Value) -> &str {
    match kind(n) {
        "text" => n.get("text").and_then(Value::as_str).unwrap_or_default(),
        "hardBreak" => "\n",
        _ => "",
    }
}

/// A text node's marks.
fn marks(node: &Value) -> &[Value] {
    node.get("marks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Puts each link on the text of `old`, inline nodes, to a document the writer can't read
/// back on the same text in `new`, the paragraph Markdown read back: on the unlinked run
/// nearest where it was, after the run the link before it went on.
// shortcut: a link whose text is gone scans the rest of the line, so a line of many such
// links is quadratic in its length; bound the scan if huge stored lines show up in a profile.
fn links(old: &[&Value], new: &mut Value, reads: Reads) {
    // Each link to put back: where its text starts in the line, the text, the mark.
    let mut runs = Vec::new();
    let mut at = 0;
    for n in old {
        let text = line_of(n);
        let hidden = marks(n).iter().find(|m| {
            is_link_mark(m)
                && m.pointer("/attrs/doc")
                    .is_some_and(|d| !read(Some(d), reads))
        });
        if let Some(mark) = hidden.filter(|_| !text.is_empty()) {
            runs.push((at, text.to_owned(), mark.clone()));
        }
        at += text.len();
    }
    let Some(content) = new.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    if runs.is_empty() {
        return;
    }
    let open: Vec<bool> = content
        .iter()
        .map(|n| kind(n) == "text" && !marks(n).iter().any(is_link_mark))
        .collect();
    let starts: Vec<usize> = content
        .iter()
        .scan(0, |at, n| {
            let start = *at;
            *at += line_of(n).len();
            Some(start)
        })
        .collect();
    // Where each link goes: by node, its text's start and length, in order.
    let mut cuts: Vec<Vec<(usize, usize, Value)>> = vec![Vec::new(); content.len()];
    let mut from = (0, 0);
    for (was, text, mark) in runs {
        let mut best: Option<(usize, usize, usize)> = None;
        for i in from.0..content.len() {
            if best.is_some_and(|(_, _, d)| starts[i] > was + d) {
                break;
            }
            if !open[i] {
                continue;
            }
            let start = if i == from.0 { from.1 } else { 0 };
            for (b, _) in line_of(&content[i])[start..].match_indices(text.as_str()) {
                let pos = starts[i] + start + b;
                if best.is_none_or(|(_, _, d)| pos.abs_diff(was) < d) {
                    best = Some((i, start + b, pos.abs_diff(was)));
                }
                if pos >= was {
                    break;
                }
            }
        }
        if let Some((i, b, _)) = best {
            from = (i, b + text.len());
            cuts[i].push((b, text.len(), mark));
        }
    }
    let piece = |n: &Value, text: &str, mark: Option<Value>| {
        let mut n = n.clone();
        n["text"] = text.into();
        if let Some(mark) = mark {
            let mut all = marks(&n).to_vec();
            all.push(mark);
            n["marks"] = Value::Array(all);
        }
        n
    };
    let mut out = Vec::with_capacity(content.len());
    for (n, cuts) in content.drain(..).zip(cuts) {
        if cuts.is_empty() {
            out.push(n);
            continue;
        }
        let whole = line_of(&n).to_owned();
        let mut pos = 0;
        for (b, len, mark) in cuts {
            if b > pos {
                out.push(piece(&n, &whole[pos..b], None));
            }
            out.push(piece(&n, &whole[b..b + len], Some(mark)));
            pos = b + len;
        }
        if pos < whole.len() {
            out.push(piece(&n, &whole[pos..], None));
        }
    }
    *content = out;
}

#[cfg(test)]
mod tests {
    use super::super::tests::{A1, M1, M2, P1, P2, doc, media_file, para, text};

    /// A file nobody in these tests reads.
    const M9: &str = "0199d5c4-0000-7000-8000-0000000000d9";
    use super::super::{from_markdown, to_markdown};
    use super::*;

    /// Links to posts and media; a `callout` block holding a text of its own.
    fn field() -> FieldKind {
        let f: Field = serde_json::from_value(json!({"name": "body", "type": "richText",
        "relationTo": ["posts", "authors", "media"],
        "upload": {"collections": {"media": {}}},
        "blocks": [{"slug": "callout", "fields": [
            {"name": "body", "type": "richText", "relationTo": ["posts"]}
        ]}]}))
        .unwrap();
        f.kind
    }

    fn rel(id: &str) -> Value {
        json!({"type": "relationship", "attrs": {"relationTo": "posts", "value": id}})
    }

    fn file(id: &str) -> Value {
        json!({"type": "upload", "attrs": {"relationTo": "media", "value": id}})
    }

    fn link(s: &str, id: &str) -> Value {
        json!({"type": "text", "text": s, "marks": [{"type": "link", "attrs": {"doc": {"relationTo": "posts", "value": id}}}]})
    }

    fn p(s: &str) -> Value {
        para(json!([text(s)]))
    }

    /// Post `P1` (slug `hello`), author `A1` and media `M1` and `M2` read; post `P2` and
    /// media `M9` don't, unless `all` reads everything.
    fn with(all: bool, test: impl FnOnce(&RenderCtx, Reads)) {
        let none = |_: &str, _: &Value| None;
        let slug = |to: &str, v: &Value| match (to, v.as_str()?) {
            ("posts", P1) => Some("hello".to_owned()),
            ("authors", A1) => Some("ann".to_owned()),
            _ => None,
        };
        let reads = move |to: &str, v: &Value| {
            all || matches!(
                (to, v.as_str()),
                ("posts", Some(P1)) | ("authors", Some(A1)) | ("media", Some(M1 | M2))
            )
        };
        test(
            &RenderCtx {
                link_url: &none,
                title: &none,
                slug: &slug,
                upload: &media_file,
            },
            &reads,
        )
    }

    /// `old` as the writer reads it in Markdown, edited by `edit`, read back and put back.
    fn round_trip(old: &Value, all: bool, edit: impl FnOnce(String) -> String) -> Value {
        let mut out = Value::Null;
        with(all, |ctx, reads| {
            let md = edit(to_markdown(old, &field(), ctx));
            let mut new = from_markdown(&md, &field()).expect("valid Markdown");
            restore_unseen(old, &mut new, &field(), ctx, reads);
            out = new;
        });
        out
    }

    #[test]
    fn what_markdown_left_out_goes_back_where_it_was() {
        let old = doc(json!([
            rel(P2),
            p("a"),
            file(M9),
            rel(P2),
            p("b"),
            p("c"),
            file(M9)
        ]));
        assert_eq!(round_trip(&old, false, |md| md), old);
        // An edit inside the block before a node keeps the node behind it.
        let fixed = round_trip(&old, false, |md| md.replace('b', "B!"));
        assert_eq!(
            fixed,
            doc(json!([
                rel(P2),
                p("a"),
                file(M9),
                rel(P2),
                p("B!"),
                p("c"),
                file(M9)
            ]))
        );
        // Its place is counted: with `a` deleted, the nodes after it follow the first block
        // the writer still has.
        let gone = round_trip(&old, false, |md| md.replacen("a\n\n", "", 1));
        assert_eq!(
            gone,
            doc(json!([
                rel(P2),
                p("b"),
                file(M9),
                rel(P2),
                p("c"),
                file(M9)
            ]))
        );
        // Past the end, it goes last.
        assert_eq!(
            round_trip(&old, false, |_| "x".into()),
            doc(json!([rel(P2), p("x"), file(M9), rel(P2), file(M9)]))
        );
        // Blocks moved: the nodes keep their count.
        assert_eq!(
            round_trip(&old, false, |_| "c\n\nb\n\na".into()),
            doc(json!([
                rel(P2),
                p("c"),
                file(M9),
                rel(P2),
                p("b"),
                p("a"),
                file(M9)
            ]))
        );
    }

    #[test]
    fn a_text_of_hidden_nodes_alone_keeps_them() {
        let old = doc(json!([rel(P2), file(M9)]));
        assert_eq!(round_trip(&old, false, |md| md), old);
        assert_eq!(
            round_trip(&old, false, |_| "new".into()),
            doc(json!([rel(P2), file(M9), p("new")]))
        );
    }

    #[test]
    fn a_writer_who_reads_everything_gets_what_they_sent() {
        let old = doc(json!([
            p("a"),
            rel(P2),
            file(M9),
            para(json!([link("x", P2)]))
        ]));
        let mut sent = Value::Null;
        with(true, |ctx, _| {
            sent = from_markdown(
                &to_markdown(&old, &field(), ctx).replace('a', "A"),
                &field(),
            )
            .unwrap();
        });
        assert_eq!(round_trip(&old, true, |md| md.replacen('a', "A", 1)), sent);
    }

    #[test]
    fn nodes_inside_go_back_inside_or_after_what_held_them() {
        let item = |content: Value| json!({"type": "listItem", "content": content});
        let list = |items: Value| json!({"type": "bulletList", "content": items});
        let quote = json!({"type": "blockquote", "content": [p("q"), rel(P2)]});
        let old = doc(json!([
            list(json!([
                item(json!([p("one")])),
                item(json!([p("two"), rel(P2)]))
            ])),
            quote
        ]));
        assert_eq!(round_trip(&old, false, |md| md), old);
        // The item holding it went: the node goes after the list. The quote is a paragraph
        // now: the node goes after it.
        assert_eq!(
            round_trip(&old, false, |_| "- one\n\nq".into()),
            doc(json!([
                list(json!([item(json!([p("one")]))])),
                rel(P2),
                p("q"),
                rel(P2)
            ]))
        );
        // Counted, not matched: with the first item gone, the second is first.
        assert_eq!(
            round_trip(&old, false, |_| "- two".into()),
            doc(json!([
                list(json!([item(json!([p("two")]))])),
                rel(P2),
                rel(P2)
            ]))
        );
    }

    #[test]
    fn a_block_nodes_text_keeps_its_own() {
        let callout = |body: Value| json!({"type": "block", "attrs": {"blockType": "callout", "fields": {"body": body}}});
        let old = doc(json!([callout(doc(json!([p("in"), rel(P2)]))), p("out")]));
        let back = round_trip(&old, false, |md| md.replace("in", "IN"));
        assert_eq!(
            back["content"][0]["attrs"]["fields"]["body"],
            doc(json!([p("IN"), rel(P2)]))
        );
        assert_eq!(back["content"][1], p("out"));
        // Without its block node, it goes with it.
        assert_eq!(
            round_trip(&old, false, |_| "out".into()),
            doc(json!([p("out")]))
        );
    }

    #[test]
    fn a_hidden_link_goes_back_on_its_text() {
        let old = doc(json!([para(json!([
            text("see the guide, "),
            link("guide", P2),
            text(" and "),
            link("this", P2)
        ]))]));
        assert_eq!(round_trip(&old, false, |md| md), old);
        let edited = round_trip(&old, false, |md| {
            md.replace("see", "See").replace("and", "or")
        });
        assert_eq!(
            edited,
            doc(json!([para(json!([
                text("See the guide, "),
                link("guide", P2),
                text(" or "),
                link("this", P2)
            ]))]))
        );
        // Text rewritten: the link goes with it.
        assert_eq!(
            round_trip(&old, false, |_| "gone".into()),
            doc(json!([p("gone")]))
        );
    }

    #[test]
    fn a_cell_takes_its_nodes_and_links_back() {
        // A table's first row reads back as its header.
        let cell = |content: Value| json!({"type": "tableHeader", "content": content});
        let table = |cells: Value| json!({"type": "table", "content": [{"type": "tableRow", "content": cells}]});
        let old = doc(json!([table(json!([cell(json!([
            para(json!([link("x", P2)])),
            file(M9)
        ]))]))]));
        assert_eq!(round_trip(&old, false, |md| md), old);
    }

    #[test]
    fn without_unseen_leaves_what_the_writer_reads() {
        let callout = json!({"type": "block", "attrs": {"blockType": "callout",
            "fields": {"body": doc(json!([rel(P2), rel(P1)]))}}});
        let mut text = doc(json!([
            rel(P2),
            file(M1),
            file(M9),
            callout,
            para(json!([link("x", P2)]))
        ]));
        with(false, |_, reads| without_unseen(&mut text, reads));
        let callout = json!({"type": "block", "attrs": {"blockType": "callout",
            "fields": {"body": doc(json!([rel(P1)]))}}});
        assert_eq!(
            text,
            doc(json!([file(M1), callout, para(json!([link("x", P2)]))]))
        );
    }
}
