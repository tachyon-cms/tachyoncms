//! Seeded round-trip fuzz for Markdown: random rich text through `to_markdown` and back
//! through `from_markdown` must come back as written, once what Markdown can't say is
//! normalised the documented way (W1-R34, R35, R37, R38, R39).

use serde_json::{Map, Value, json};

use super::{RenderCtx, from_markdown, slug_refs_mut, to_markdown, to_text};
use crate::schema::{Field, FieldKind};

/// A callout holding text (which may hold an aside holding text) and a video leaf.
fn field() -> Field {
    serde_json::from_value(json!({"name": "body", "type": "richText",
        "relationTo": ["posts", "authors"], "blocks": [
        {"slug": "callout", "fields": [
            {"name": "kind", "type": "text"},
            {"name": "body", "type": "richText", "relationTo": ["posts"], "blocks": [
                {"slug": "aside", "fields": [
                    {"name": "tone", "type": "text"},
                    {"name": "text", "type": "richText"}]}
            ]}
        ]},
        {"slug": "video", "fields": [
            {"name": "url", "type": "text", "required": true},
            {"name": "start", "type": "number"},
            {"name": "autoplay", "type": "boolean"},
            {"name": "meta", "type": "json"}
        ]}
    ]}))
    .unwrap()
}

/// xorshift64: the same cases every run.
struct Rng(u64);

impl Rng {
    fn n(&mut self, m: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % m as u64) as usize
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.n(xs.len())]
    }
}

/// Markdown's punctuation and markers, line breaks, entities and awkward Unicode.
const FRAGS: &[&str] = &[
    "a",
    "b",
    "1",
    " ",
    "\t",
    "*",
    "_",
    "~",
    "`",
    "``",
    "[",
    "]",
    "]:",
    "!",
    "&",
    "#",
    "\\",
    "<",
    ">",
    "|",
    "é",
    "\u{2014}",
    "\u{24B6}",
    "e\u{301}",
    "\u{a0}",
    "中",
    "(",
    ")",
    ":",
    "-",
    "=",
    "+",
    ".",
    "'",
    "\"",
    "\u{200b}",
    "\u{3000}",
    "$",
    "\u{20ac}",
    "<u>",
    "<br>",
    "**",
    "__",
    "&amp;",
    "&#32;",
    "\u{ff01}",
    "\u{2028}",
    "\u{85}",
    "\u{c}",
    "\u{b}",
    "\u{ad}",
    "\n",
    "\r",
    "\r\n",
    "<br/>",
    "<BR>",
    "::",
    ":::",
    "{",
    "}",
    "doc:",
    "]: x \"",
    "]: x '",
    "\u{301}",
    "\u{2029}",
    "\u{feff}",
    "&#10;",
    "<script>",
    "[x](/y)",
    "<!--",
    "-->",
    "1.",
    "- ",
    "> ",
    "   ",
    "\\\\",
    "&lt;br&gt;",
    "\u{1F600}",
    "<https://x.y>",
    "www.x.com",
];

/// Hostile directive attribute values.
const VALS: &[&str] = &[
    "a",
    " ",
    "\"",
    "\r\n",
    "\r",
    "\n",
    "\\",
    "\\n",
    "}",
    "{",
    "=",
    "é",
    "中",
    "\t",
    "::",
    ":::",
    "<br>",
    "<script>",
    "]:",
    "#",
    "\u{85}",
    "\u{2028}",
    "`",
    "```",
    "&amp;",
    "x y",
    "\u{1F600}",
    "\"}",
    "\\\"",
];

/// Documents links may name: collection, ID and slug.
const DOCS: &[(&str, &str, &str)] = &[
    ("posts", "0199d5c4-0000-7000-8000-0000000000a1", "hello"),
    (
        "posts",
        "0199d5c4-0000-7000-8000-0000000000a2",
        "my-first-post",
    ),
    ("authors", "0199d5c4-0000-7000-8000-0000000000b1", "ann"),
    ("authors", "0199d5c4-0000-7000-8000-0000000000b2", "b-2"),
];

fn text(r: &mut Rng) -> String {
    (0..1 + r.n(3)).map(|_| r.pick(FRAGS)).collect()
}

fn val(r: &mut Rng) -> String {
    (0..r.n(4)).map(|_| r.pick(VALS)).collect()
}

fn target(r: &mut Rng, rels: &[&str]) -> Option<(&'static str, &'static str)> {
    let found: Vec<_> = DOCS.iter().filter(|d| rels.contains(&d.0)).collect();
    match found.is_empty() {
        true => None,
        false => {
            let d = found[r.n(found.len())];
            Some((d.0, d.1))
        }
    }
}

fn br() -> Value {
    json!({"type": "hardBreak"})
}

fn para(content: Vec<Value>) -> Value {
    json!({"type": "paragraph", "content": content})
}

/// Text nodes under random marks, with hard breaks anywhere.
fn inline(r: &mut Rng, rels: &[&str], breaks: bool) -> Vec<Value> {
    let mut nodes = Vec::new();
    for i in 0..1 + r.n(6) {
        if breaks && r.n(5) == 0 && (i > 0 || r.n(3) == 0) {
            nodes.push(br());
            if r.n(4) == 0 {
                nodes.push(br());
            }
        }
        let s = text(r);
        let mut marks = Vec::new();
        for m in ORDER {
            if r.n(3) == 0 {
                marks.push(match (m, r.n(3), target(r, rels)) {
                    ("link", 0, Some((to, v))) => {
                        json!({"type": "link", "attrs": {"doc": {"relationTo": to, "value": v}}})
                    }
                    ("link", ..) => {
                        let href = r.pick(&["/a", "/b", "https://x.y/(p)", "#t", "/a|b", "/]:"]);
                        json!({"type": "link", "attrs": {"href": href}})
                    }
                    (m, ..) => json!({"type": m}),
                });
            }
        }
        nodes.push(match marks.is_empty() {
            true => json!({"type": "text", "text": s}),
            false => json!({"type": "text", "text": s, "marks": marks}),
        });
    }
    if breaks && r.n(8) == 0 {
        nodes.push(br());
    }
    nodes
}

/// Which rich text a block sits in: the field's, a callout's or an aside's.
#[derive(Clone, Copy)]
enum In {
    Root,
    Callout,
    Aside,
}

impl In {
    fn rels(self) -> &'static [&'static str] {
        match self {
            In::Root => &["posts", "authors"],
            In::Callout => &["posts"],
            In::Aside => &[],
        }
    }
}

fn blocks(r: &mut Rng, at: In, depth: usize, max: usize) -> Vec<Value> {
    (0..1 + r.n(max)).map(|_| block(r, at, depth)).collect()
}

fn block(r: &mut Rng, at: In, depth: usize) -> Value {
    let rels = at.rels();
    match r.n(if depth >= 3 { 4 } else { 14 }) {
        0 | 1 => para(inline(r, rels, true)),
        2 => {
            json!({"type": "heading", "attrs": {"level": 1 + r.n(6)}, "content": inline(r, rels, true)})
        }
        3 => {
            let parts = [
                "a", "\n", "```", "~~~", " ", "\t", "<br>", ":::", "\r\n", "é", "\r",
            ];
            let code: String = (0..r.n(4)).map(|_| r.pick(&parts)).collect();
            let mut n = json!({"type": "codeBlock"});
            if !code.is_empty() {
                n["content"] = json!([{"type": "text", "text": code}]);
            }
            if r.n(2) == 0 {
                n["attrs"] = json!({"language": r.pick(&["rust", "c++", "a_b-c"])});
            }
            n
        }
        4 => json!({"type": "horizontalRule"}),
        5 => json!({"type": "blockquote", "content": blocks(r, at, depth + 1, 3)}),
        6 | 7 => {
            let kind = r.pick(&["bulletList", "orderedList", "taskList"]);
            let items: Vec<Value> = (0..1 + r.n(3))
                .map(|_| {
                    let mut content = vec![match r.n(4) {
                        0 => block(r, at, depth + 1),
                        _ => para(inline(r, rels, true)),
                    }];
                    if r.n(3) == 0 {
                        content.push(block(r, at, depth + 1));
                    }
                    match kind {
                        "taskList" => json!({"type": "taskItem", "attrs": {"checked": r.n(2) == 0}, "content": content}),
                        _ => json!({"type": "listItem", "content": content}),
                    }
                })
                .collect();
            let mut n = json!({"type": kind, "content": items});
            if kind == "orderedList" && r.n(3) == 0 {
                n["attrs"] = json!({"start": r.n(4)});
            }
            n
        }
        8 => {
            let cols = 1 + r.n(3);
            let rows: Vec<Value> = (0..1 + r.n(3))
                .map(|i| {
                    let t = if i == 0 { "tableHeader" } else { "tableCell" };
                    let cells: Vec<Value> = (0..cols)
                        .map(|_| {
                            let content = match r.n(4) {
                                0 => blocks(r, at, 3, 2),
                                _ => vec![para(inline(r, rels, true))],
                            };
                            json!({"type": t, "content": content})
                        })
                        .collect();
                    json!({"type": "tableRow", "content": cells})
                })
                .collect();
            json!({"type": "table", "content": rows})
        }
        9 => match target(r, rels) {
            Some((to, v)) => {
                json!({"type": "relationship", "attrs": {"relationTo": to, "value": v}})
            }
            None => para(inline(r, rels, false)),
        },
        // A paragraph that is only hard breaks: an empty line in an editor.
        10 => para((0..1 + r.n(2)).map(|_| br()).collect()),
        _ => match at {
            In::Root if r.n(2) == 0 => {
                let mut fields = Map::new();
                fields.insert("url".into(), format!("u{}", val(r)).into());
                if r.n(2) == 0 {
                    fields.insert(
                        "start".into(),
                        [json!(30), json!(-2), json!(1.5), json!(0)][r.n(4)].clone(),
                    );
                }
                if r.n(2) == 0 {
                    fields.insert("autoplay".into(), json!(r.n(2) == 0));
                }
                if r.n(2) == 0 {
                    let s = val(r);
                    let meta = [
                        json!(s),
                        json!({"k": s, "n": [1, null]}),
                        json!(7),
                        json!({"type": "doc", "content": []}),
                    ];
                    fields.insert("meta".into(), meta[r.n(4)].clone());
                }
                json!({"type": "block", "attrs": {"blockType": "video", "fields": fields}})
            }
            In::Root => {
                let mut fields = Map::new();
                if r.n(2) == 0 {
                    fields.insert("kind".into(), val(r).into());
                }
                if r.n(5) != 0 {
                    fields.insert(
                        "body".into(),
                        json!({"type": "doc", "content": blocks(r, In::Callout, depth + 1, 3)}),
                    );
                }
                json!({"type": "block", "attrs": {"blockType": "callout", "fields": fields}})
            }
            In::Callout => {
                let mut fields = Map::new();
                if r.n(2) == 0 {
                    fields.insert("tone".into(), val(r).into());
                }
                fields.insert(
                    "text".into(),
                    json!({"type": "doc", "content": blocks(r, In::Aside, depth + 1, 3)}),
                );
                json!({"type": "block", "attrs": {"blockType": "aside", "fields": fields}})
            }
            In::Aside => para(inline(r, rels, true)),
        },
    }
}

/// The reader's mark order (W1-R37).
const ORDER: [&str; 6] = ["bold", "italic", "underline", "strike", "link", "code"];

/// `v` as Markdown gives it back: a line break in code text loses the code mark (R28);
/// same-marked neighbours merge (R34); marks in one order (R37); a table cell is one
/// paragraph, code blocks end lines with `\n`, `start: 1` and empty `fields` go (R39).
fn normal(v: &Value) -> Value {
    let Value::Object(o) = v else {
        return match v {
            Value::Array(a) => Value::Array(a.iter().map(normal).collect()),
            v => v.clone(),
        };
    };
    let mut o = o.clone();
    let kind = o
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if matches!(kind.as_str(), "tableCell" | "tableHeader") {
        let line = cell_inline(&o["content"]);
        let p = match line.is_empty() {
            true => json!({"type": "paragraph"}),
            false => para(line),
        };
        o.insert("content".into(), json!([p]));
    }
    if kind == "codeBlock"
        && let Some(Value::String(t)) = o.get_mut("content").and_then(|c| c.pointer_mut("/0/text"))
    {
        *t = t.replace("\r\n", "\n").replace('\r', "\n");
    }
    for (_, x) in o.iter_mut() {
        *x = normal(x);
    }
    if o.get("fields")
        .is_some_and(|f| f.as_object().is_some_and(Map::is_empty))
    {
        o.remove("fields");
    }
    if kind == "orderedList"
        && let Some(Value::Object(a)) = o.get_mut("attrs")
    {
        if a.get("start") == Some(&json!(1)) {
            a.remove("start");
        }
        if a.is_empty() {
            o.remove("attrs");
        }
    }
    if kind != "codeBlock"
        && let Some(Value::Array(content)) = o.get("content").cloned()
    {
        o.insert("content".into(), Value::Array(merged(content)));
    }
    Value::Object(o)
}

/// A cell's blocks as one line of inline nodes, as Markdown writes a cell (R37, R39).
fn cell_inline(content: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, block) in content.as_array().into_iter().flatten().enumerate() {
        if i > 0 {
            out.push(br());
        }
        match block["type"].as_str() {
            Some("paragraph" | "heading") => {
                out.extend(block.get("content").and_then(Value::as_array).into_iter().flatten().cloned())
            }
            _ => out.push(json!({"type": "text", "text": to_text(&json!({"type": "doc", "content": [block]}))})),
        }
    }
    out.retain(|n| n["type"] != "text" || n["text"] != "");
    out
}

/// Inline nodes split where code text breaks a line, marks in order, neighbours merged.
fn merged(content: Vec<Value>) -> Vec<Value> {
    let mut split = Vec::new();
    for node in content {
        let code = node
            .get("marks")
            .and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(|m| m["type"] == "code"));
        let text = node
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if !code || !text.contains(['\n', '\r']) {
            split.push(node);
            continue;
        }
        let mut run = String::new();
        let flush = |run: &mut String, split: &mut Vec<Value>| {
            if !run.is_empty() {
                let mut n = node.clone();
                n["text"] = std::mem::take(run).into();
                split.push(n);
            }
        };
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                flush(&mut run, &mut split);
                let mut n = node.clone();
                n["text"] = ch.to_string().into();
                let marks: Vec<Value> = n["marks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["type"] != "code")
                    .cloned()
                    .collect();
                match marks.is_empty() {
                    true => drop(n.as_object_mut().unwrap().remove("marks")),
                    false => n["marks"] = marks.into(),
                }
                split.push(n);
            } else {
                run.push(ch);
            }
        }
        flush(&mut run, &mut split);
    }
    let mut out: Vec<Value> = Vec::new();
    for mut node in split {
        if let Some(Value::Array(m)) = node.get_mut("marks") {
            m.sort_by_key(|m| ORDER.iter().position(|o| m["type"] == *o));
        }
        match out.last_mut() {
            Some(last)
                if last["type"] == "text"
                    && node["type"] == "text"
                    && last.get("marks") == node.get("marks") =>
            {
                let joined = format!(
                    "{}{}",
                    last["text"].as_str().unwrap(),
                    node["text"].as_str().unwrap()
                );
                last["text"] = joined.into();
            }
            _ => out.push(node),
        }
    }
    out
}

/// Runs `cases` documents from `make` through Markdown and back, and fails with the first
/// few that don't come back as `normal` says they should.
fn run(seed: u64, cases: usize, make: impl Fn(&mut Rng, usize) -> Value) {
    let kind: FieldKind = field().kind;
    let none = |_: &str, _: &Value| None;
    let slug = |to: &str, v: &Value| {
        DOCS.iter()
            .find(|d| d.0 == to && Some(d.1) == v.as_str())
            .map(|d| d.2.to_owned())
    };
    let ctx = RenderCtx {
        link_url: &none,
        title: &none,
        slug: &slug,
    };
    let mut r = Rng(seed);
    let mut failures = Vec::new();
    for case in 0..cases {
        let doc = make(&mut r, case);
        let markdown = to_markdown(&doc, &kind, &ctx);
        let back = from_markdown(&markdown, &kind).map(|mut back| {
            slug_refs_mut(&mut back, &mut |_, link| {
                let slug = link.remove("slug").unwrap();
                link.remove("line");
                let to = link["relationTo"].as_str().unwrap().to_owned();
                let id = DOCS
                    .iter()
                    .find(|d| d.0 == to && d.2 == slug)
                    .map_or("?", |d| d.1);
                link.insert("value".into(), id.into());
            });
            back
        });
        let want = normal(&doc);
        match &back {
            Ok(got) if *got == want => {}
            Ok(got) => failures.push(format!(
                "{}\n{markdown:?}",
                first_difference(&want, got, "")
            )),
            Err(e) => failures.push(format!("{e:?}\n{markdown:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} failed; first:\n{}",
        failures.len(),
        failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// Where `want` and `got` first differ, and how.
fn first_difference(want: &Value, got: &Value, at: &str) -> String {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            for k in w.keys().chain(g.keys()) {
                if w.get(k) != g.get(k) {
                    return match (w.get(k), g.get(k)) {
                        (Some(w), Some(g)) => first_difference(w, g, &format!("{at}/{k}")),
                        (w, g) => format!("{at}/{k}: want {w:?}, got {g:?}"),
                    };
                }
            }
            String::new()
        }
        (Value::Array(w), Value::Array(g)) => w
            .iter()
            .zip(g)
            .enumerate()
            .find(|(_, (w, g))| w != g)
            .map(|(i, (w, g))| first_difference(w, g, &format!("{at}/{i}")))
            .unwrap_or_else(|| format!("{at}: want {} items, got {}", w.len(), g.len())),
        _ => format!("{at}: want {want}, got {got}"),
    }
}

/// Inline text in every context a line of it can sit in, beside break-only paragraphs.
#[test]
fn markdown_fuzz_round_trips() {
    run(0x9e37_79b9_7f4a_7c15, 15_000, |r, case| {
        let content = inline(r, In::Root.rels(), true);
        let mut paras = vec![para(content.clone())];
        if r.n(3) == 0 {
            paras.insert(r.n(2), para(vec![br()]));
        }
        let body = |paras: Vec<Value>| json!({"type": "doc", "content": paras});
        let wrapped = match case % 6 {
            0 => paras,
            1 => vec![json!({"type": "heading", "attrs": {"level": 2}, "content": content})],
            2 => vec![json!({"type": "blockquote", "content": paras})],
            3 => vec![
                json!({"type": "orderedList", "content": [{"type": "listItem", "content": paras}]}),
            ],
            4 => vec![
                json!({"type": "table", "content": [{"type": "tableRow", "content": [
                    {"type": "tableHeader", "content": [para(content)]}
                ]}]}),
            ],
            _ => {
                // A callout's text may only link to posts.
                let mut paras = paras;
                for p in &mut paras {
                    strip_links(p);
                }
                vec![
                    json!({"type": "block", "attrs": {"blockType": "callout", "fields": {"body": body(paras)}}}),
                ]
            }
        };
        body(wrapped)
    });
}

/// Drops doc links to anything but posts from `v`'s marks.
fn strip_links(v: &mut Value) {
    if let Some(Value::Array(content)) = v.get_mut("content") {
        for n in content {
            if let Some(Value::Array(marks)) = n.get_mut("marks") {
                marks.retain(|m| {
                    m.pointer("/attrs/doc/relationTo")
                        .is_none_or(|t| t == "posts")
                });
                if marks.is_empty() {
                    n.as_object_mut().unwrap().remove("marks");
                }
            }
        }
    }
}

/// Whole documents: lists, task lists and nesting, quotes, tables, code, directives with
/// hostile attributes, references, doc links and break-only paragraphs.
#[test]
fn markdown_block_fuzz_round_trips() {
    run(
        0x5dee_ce66_d1ce_4e5b,
        2_500,
        |r, _| json!({"type": "doc", "content": blocks(r, In::Root, 0, 4)}),
    );
}
