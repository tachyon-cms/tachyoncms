//! Scaling guard (W1-R46): every render and parse path, on the worst shapes known, at the
//! node limit. Each run stays inside a generous debug bound, doubling the input must not
//! triple the time, and the loops that once went quadratic count their steps.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    MAX_DEPTH, MAX_NODES, RenderCtx, from_markdown, to_html, to_markdown, to_text, validate,
};
use crate::schema::{Field, FieldKind};

thread_local!(pub(super) static STEPS: Cell<usize> = const { Cell::new(0) });

/// The most one run may take, in a debug build: loose enough for a loaded CI runner, since
/// the step counts are the guard that a quadratic loop can't pass (W1-R47).
const BOUND: Duration = Duration::from_secs(10);
/// Below this, a run is too quick for its growth to matter or to time reliably.
const QUICK: Duration = Duration::from_millis(50);

/// A field that turns every feature on, links to posts and offers a `cta` block.
fn kind() -> FieldKind {
    let f: Field = serde_json::from_value(json!({"name": "body", "type": "richText",
        "relationTo": ["posts"],
        "blocks": [{"slug": "cta", "fields": [{"name": "label", "type": "text"}]}]}))
    .unwrap();
    f.kind
}

fn doc(content: Vec<Value>) -> Value {
    json!({"type": "doc", "content": content})
}

fn text(s: &str, marks: Value) -> Value {
    json!({"type": "text", "text": s, "marks": marks})
}

/// `n` nodes of headings with the same text.
fn same_headings(n: usize) -> Value {
    let h =
        json!({"type": "heading", "attrs": {"level": 2}, "content": [text("Intro", json!([]))]});
    doc(vec![h; n / 2])
}

/// `n` empty headings: two bytes each in Markdown.
fn empty_headings(n: usize) -> Value {
    doc(vec![json!({"type": "heading", "attrs": {"level": 1}}); n])
}

/// One paragraph of one-character texts, each under different marks from the last.
fn marked_runs(n: usize) -> Value {
    let marks = [
        json!([{"type": "bold"}]),
        json!([{"type": "italic"}]),
        json!([{"type": "code"}]),
        json!([{"type": "link", "attrs": {"href": "/a"}}]),
        json!([{"type": "strike"}]),
        json!([{"type": "bold"}, {"type": "italic"}]),
        json!([{"type": "underline"}]),
        json!([{"type": "link", "attrs": {"href": "/b"}}]),
    ];
    let texts = ["a", ".", "é", "_"];
    let content: Vec<Value> = (0..n - 1)
        .map(|i| text(texts[i % texts.len()], marks[i % marks.len()].clone()))
        .collect();
    doc(vec![json!({"type": "paragraph", "content": content})])
}

/// Blockquotes nested to the depth limit around paragraphs of text.
fn deep(n: usize) -> Value {
    let p = json!({"type": "paragraph", "content": [text("deep", json!([]))]});
    let mut node = json!({"type": "blockquote", "content": vec![p; (n - MAX_DEPTH) / 2]});
    for _ in 0..MAX_DEPTH - 3 {
        node = json!({"type": "blockquote", "content": [node]});
    }
    doc(vec![node])
}

/// `n` block nodes, each with an ID and a field.
fn blocks(n: usize) -> Value {
    doc((0..n)
        .map(|i| {
            let id = format!("0199d5c4-0000-7000-8000-{i:012x}");
            json!({"type": "block", "attrs": {"blockType": "cta", "id": id, "fields": {"label": "Go"}}})
        })
        .collect())
}

/// One paragraph of touching links, alternating between two places.
fn links(n: usize) -> Value {
    let content: Vec<Value> = (0..n - 1)
        .map(|i| {
            let href = ["https://example.com/a", "/b"][i % 2];
            text("x", json!([{"type": "link", "attrs": {"href": href}}]))
        })
        .collect();
    doc(vec![json!({"type": "paragraph", "content": content})])
}

/// One paragraph of text and hard breaks, one after the other.
fn breaks(n: usize) -> Value {
    let content: Vec<Value> = (0..n - 1)
        .map(|i| match i % 2 {
            0 => text("a", json!([])),
            _ => json!({"type": "hardBreak"}),
        })
        .collect();
    doc(vec![json!({"type": "paragraph", "content": content})])
}

/// One paragraph of non-ASCII text, bold and plain in turn: every run needs a flank fix.
fn flanked(n: usize) -> Value {
    let content: Vec<Value> = (0..n - 1)
        .map(|i| match i % 2 {
            0 => text("é", json!([{"type": "bold"}])),
            _ => text("é", json!([])),
        })
        .collect();
    doc(vec![json!({"type": "paragraph", "content": content})])
}

type Shape = fn(usize) -> Value;

const SHAPES: &[(&str, Shape)] = &[
    ("same-text headings", same_headings),
    ("empty headings", empty_headings),
    ("marked runs", marked_runs),
    ("deep nesting", deep),
    ("block nodes", blocks),
    ("links", links),
    ("hard breaks", breaks),
    ("flank fixes", flanked),
];

/// Each shape at the node limit and at half of it.
fn corpus() -> &'static [(&'static str, Value, Value)] {
    static CORPUS: OnceLock<Vec<(&str, Value, Value)>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        SHAPES
            .iter()
            .map(|(name, shape)| (*name, shape(MAX_NODES), shape(MAX_NODES / 2)))
            .collect()
    })
}

fn render<T>(f: impl FnOnce(&RenderCtx) -> T) -> T {
    let none = |_: &str, _: &Value| None;
    f(&RenderCtx {
        link_url: &none,
        title: &none,
        slug: &none,
    })
}

/// How long `op` takes on `input`, the best of `runs`, and the steps it counts a run.
fn measure<T>(input: &T, op: &impl Fn(&T), runs: usize) -> (Duration, usize) {
    STEPS.with(|s| s.set(0));
    let mut best = Duration::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        op(input);
        best = best.min(start.elapsed());
    }
    (best, STEPS.with(Cell::get) / runs)
}

/// Runs `op` on each shape's `input`, made from the full shape and the half, which is also
/// told it's the half: the full one inside `BOUND`, at most 3x the half's time, and at most
/// 8 counted steps a node.
fn guard<T>(path: &str, input: impl Fn(&Value, bool) -> T, op: impl Fn(&T)) {
    for (name, full, half) in corpus() {
        let (full, half) = (input(full, false), input(half, true));
        // The full one takes its best of two; the half runs once, to keep the guard quick.
        let (t_full, steps) = measure(&full, &op, 2);
        let (t_half, _) = measure(&half, &op, 1);
        eprintln!("{path} on {name}: {t_full:?} (half: {t_half:?}, steps: {steps})");
        // Steps first: they're the guard a machine's speed can't sway (W1-R47).
        assert!(
            steps <= 8 * MAX_NODES,
            "{path} on {name} took {steps} steps for {MAX_NODES} nodes"
        );
        assert!(t_full < BOUND, "{path} on {name} took {t_full:?}");
        assert!(
            t_full < t_half * 3 || t_full < QUICK,
            "{path} on {name} grows faster than its input: {t_half:?}, then {t_full:?}"
        );
    }
}

#[test]
fn the_shapes_are_legal_and_at_the_limit() {
    let count = |v: &Value| {
        let mut n = 0;
        super::each_node(v, &mut |_| n += 1);
        n - 1
    };
    for (name, full, half) in corpus() {
        assert!(validate(full, &kind(), "body").is_ok(), "{name}");
        let n = count(full);
        assert!(
            n > MAX_NODES * 99 / 100 && n <= MAX_NODES,
            "{name}: {n} nodes"
        );
        assert!(count(half) <= n / 2 + 1, "{name}");
    }
}

#[test]
fn html_scales_with_its_input() {
    guard(
        "to_html",
        |v, _| v.clone(),
        |v| {
            render(|ctx| to_html(v, ctx));
        },
    );
}

#[test]
fn markdown_scales_with_its_input() {
    let kind = kind();
    guard(
        "to_markdown",
        |v, _| v.clone(),
        |v| {
            render(|ctx| to_markdown(v, &kind, ctx));
        },
    );
}

#[test]
fn text_scales_with_its_input() {
    guard(
        "to_text",
        |v, _| v.clone(),
        |v| {
            to_text(v);
        },
    );
}

#[test]
fn validation_scales_with_its_input() {
    let kind = kind();
    guard(
        "validate",
        |v, _| v.clone(),
        |v| {
            assert!(validate(v, &kind, "body").is_ok());
        },
    );
}

/// From each shape's own Markdown, cut at a line end to the 1 MiB Markdown may be at most
/// (the half to half that), so the parser does the whole job rather than refusing it.
#[test]
fn markdown_input_scales_with_its_input() {
    let kind = kind();
    let fit = |v: &Value, half: bool| {
        let md = render(|ctx| to_markdown(v, &kind, ctx));
        let most = if half { 1 << 19 } else { 1 << 20 };
        match md.len() <= most {
            true => md,
            false => md[..md.as_bytes()[..most]
                .iter()
                .rposition(|&b| b == b'\n')
                .unwrap()]
                .to_owned(),
        }
    };
    guard("from_markdown", fit, |md| {
        assert!(from_markdown(md, &kind).is_ok());
    });
}
