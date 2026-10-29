//! Markdown in: GitHub-flavoured Markdown, with the extensions `to_markdown` writes
//! (`:::block{…}` directives, `::ref{…}`, `doc:` links and `<u>`), read into rich text.

use std::collections::HashMap;
use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{MAX_DEPTH, MAX_NODES};
use crate::query::{ValueKind, value_kind};
use crate::schema::{Field, FieldKind};
use crate::validate::FieldError;

const HTML: &str = "raw HTML isn't allowed; underline is the only tag, as <u>…</u>";

/// The order a text node's marks are kept in.
const MARKS: &[&str] = &["bold", "italic", "underline", "strike", "link", "code"];

/// The most Markdown one value may hold, read before parsing: the input is untrusted.
const MAX_BYTES: usize = 1 << 20;

/// A refusal, on a line of the source.
type Refusal = (usize, String);

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// `src` as rich text for a field of `kind`, checked as `validate` checks it. Each error
/// but the size limit's starts with the line it's on (`line 3: …`); its path is the
/// node's, or empty when the Markdown itself can't be read. A document named by slug
/// (`doc:posts/intro`, `::ref`) is left as `{"relationTo", "slug", "line"}` for the
/// caller to find by what the writer may read; see `slug_refs_mut`.
pub fn from_markdown(src: &str, kind: &FieldKind) -> Result<Value, Vec<FieldError>> {
    if src.len() > MAX_BYTES {
        return Err(vec![FieldError {
            path: String::new(),
            message: format!("Markdown can be {MAX_BYTES} bytes at most"),
        }]);
    }
    // A lone `\r` ends a line, as in CommonMark; as `\n`, lines are counted alike. One
    // byte for another, so offsets don't move.
    let bytes = src.as_bytes();
    let src: String = src
        .char_indices()
        .map(|(i, c)| match c {
            '\r' if bytes.get(i + 1) != Some(&b'\n') => '\n',
            c => c,
        })
        .collect();
    let mut doc = Reader::new(&src, kind).read().map_err(|(line, message)| {
        vec![FieldError {
            path: String::new(),
            message: format!("line {line}: {message}"),
        }]
    })?;
    // Nodes carry their line until here, where it's moved aside for the errors below.
    let mut lines = Vec::new();
    each_node_at(&mut doc, "", &mut |path, node| {
        if let Some(line) = node.remove("line").and_then(|l| l.as_u64()) {
            lines.push((path.to_owned(), line));
        }
    });
    // Checked as though every slug named a document: whether one does is the caller's call.
    let mut check = doc.clone();
    slug_refs_mut(&mut check, &mut |_, link| {
        link.remove("slug");
        link.remove("line");
        link.insert("value".into(), Uuid::nil().to_string().into());
    });
    crate::validate::block_defaults(kind, &mut check);
    super::validate(&check, kind, "").map_err(|errors| {
        errors
            .into_iter()
            .map(|e| {
                let path = e.path.trim_start_matches('.').to_owned();
                let line = lines
                    .iter()
                    .filter(|(p, _)| path == *p || path.starts_with(&format!("{p}.")))
                    .max_by_key(|(p, _)| p.len())
                    .map_or(1, |(_, line)| *line);
                FieldError {
                    message: format!("line {line}: {}", e.message),
                    path,
                }
            })
            .collect::<Vec<_>>()
    })?;
    Ok(doc)
}

/// Calls `f` on each document `from_markdown`'s output names by slug, with where it sits
/// (`content.0.content.1.marks.0.attrs.doc`, `content.2.attrs`), at any depth, block
/// nodes' text included. `f` gets `{"relationTo", "slug", "line"}` to replace with `value`.
pub fn slug_refs_mut(value: &mut Value, f: &mut impl FnMut(&str, &mut Map<String, Value>)) {
    each_node_at(value, "", &mut |path, node| {
        if let Some(Value::Array(marks)) = node.get_mut("marks") {
            for (m, mark) in marks.iter_mut().enumerate() {
                if let Some(Value::Object(doc)) = mark.pointer_mut("/attrs/doc")
                    && doc.contains_key("slug")
                {
                    f(&join(path, &format!("marks.{m}.attrs.doc")), doc);
                }
            }
        }
        if node.get("type").and_then(Value::as_str) == Some("relationship")
            && let Some(Value::Object(attrs)) = node.get_mut("attrs")
            && attrs.contains_key("slug")
        {
            f(&join(path, "attrs"), attrs);
        }
    });
}

fn join(path: &str, rest: &str) -> String {
    match path {
        "" => rest.to_owned(),
        _ => format!("{path}.{rest}"),
    }
}

/// Calls `f` on `node` and every node inside it, the text of block nodes included, each
/// with its path as validation names it.
fn each_node_at(node: &mut Value, path: &str, f: &mut impl FnMut(&str, &mut Map<String, Value>)) {
    let Value::Object(obj) = node else { return };
    f(path, obj);
    if let Some(Value::Array(children)) = obj.get_mut("content") {
        for (i, child) in children.iter_mut().enumerate() {
            each_node_at(child, &join(path, &format!("content.{i}")), f);
        }
    }
    if obj.get("type").and_then(Value::as_str) == Some("block")
        && let Some(Value::Object(fields)) = obj.get_mut("attrs").and_then(|a| a.get_mut("fields"))
    {
        for (name, text) in fields.iter_mut() {
            if text.get("type").and_then(Value::as_str) == Some("doc") {
                each_node_at(text, &join(path, &format!("attrs.fields.{name}")), f);
            }
        }
    }
}

/// A node of type `t` from `line`, which `from_markdown` moves aside once the tree is built.
fn node(t: &str, line: usize) -> Map<String, Value> {
    let mut n = Map::new();
    n.insert("type".into(), t.into());
    n.insert("line".into(), line.into());
    n
}

struct Reader<'s, 'k> {
    src: &'s str,
    /// Where each line starts.
    starts: Vec<usize>,
    /// The lines holding a directive, each with where its colons start, until it's read.
    directives: HashMap<usize, usize>,
    root: &'k FieldKind,
    stack: Vec<Frame<'k>>,
    /// The marks on text read now, the outermost first.
    marks: Vec<Value>,
    nodes: usize,
}

struct Frame<'k> {
    line: usize,
    open: Open<'k>,
    children: Vec<Value>,
}

enum Open<'k> {
    /// The document.
    Doc,
    /// A node that takes its children when it ends.
    Node(Map<String, Value>),
    /// A paragraph Markdown leaves unsaid, in a tight list item or a table cell.
    Loose,
    /// A list: whether each item read so far was a task, and how.
    List {
        start: Option<u64>,
        tasks: Vec<Option<bool>>,
    },
    /// An item, and its task marker once read.
    Item(Option<bool>),
    TableRow {
        head: bool,
    },
    Code {
        language: Option<String>,
        text: String,
    },
    /// A directive's line, read from the source when it ends.
    Directive,
    /// A block directive's text, until the line that closes it. `text` is the block's one
    /// rich text field, which holds it.
    Body {
        colons: usize,
        block_type: String,
        fields: Map<String, Value>,
        text: Option<&'k Field>,
    },
}

impl<'s, 'k> Reader<'s, 'k> {
    fn new(src: &'s str, root: &'k FieldKind) -> Self {
        let starts = std::iter::once(0)
            .chain(src.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self {
            src,
            starts,
            directives: HashMap::new(),
            root,
            stack: vec![Frame {
                line: 1,
                open: Open::Doc,
                children: Vec::new(),
            }],
            marks: Vec::new(),
            nodes: 0,
        }
    }

    /// The 1-based line `offset` is on.
    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset)
    }

    /// `src` with each directive line's colons turned into an ATX heading's `#` and spaces,
    /// at the same offsets, so the parser reads it as a line of its own wherever it sits (in
    /// a list item, a quote) and can't run it into the lines around it. Lines in code and
    /// HTML blocks are left as they are.
    fn patch(&mut self) -> String {
        let verbatim: Vec<Range<usize>> = Parser::new_ext(self.src, options())
            .into_offset_iter()
            .filter(|(e, _)| matches!(e, Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock)))
            .map(|(_, r)| r)
            .collect();
        let mut out = self.src.as_bytes().to_vec();
        for (n, &start) in self.starts.iter().enumerate() {
            let end = self.starts.get(n + 1).copied().unwrap_or(self.src.len());
            let Some(at) = directive_at(&self.src[start..end]).map(|i| start + i) else {
                continue;
            };
            // Sorted and apart, as blocks are.
            let i = verbatim.partition_point(|r| r.end <= at);
            if verbatim.get(i).is_some_and(|r| r.contains(&at)) {
                continue;
            }
            let colons = out[at..].iter().take_while(|&&b| b == b':').count();
            out[at] = b'#';
            out[at + 1..at + colons].fill(b' ');
            self.directives.insert(n + 1, at);
        }
        String::from_utf8(out).expect("only ASCII colons were replaced")
    }

    fn read(mut self) -> Result<Value, Refusal> {
        let patched = self.patch();
        for (event, range) in Parser::new_ext(&patched, options()).into_offset_iter() {
            let line = self.line(range.start);
            // A directive's line is read from the source, not as a heading's text.
            if matches!(self.top().open, Open::Directive)
                && !matches!(event, Event::End(TagEnd::Heading(_)))
            {
                continue;
            }
            self.event(event, line)?;
        }
        if let Some(&line) = self.directives.keys().min() {
            return Err((
                line,
                "a directive must start its own line, indented less than four spaces".into(),
            ));
        }
        if let Some(Frame {
            line,
            open: Open::Body {
                colons, block_type, ..
            },
            ..
        }) = self.stack.last()
        {
            return Err((
                *line,
                format!("`{}{block_type}` is never closed", ":".repeat(*colons)),
            ));
        }
        let root = self.stack.pop().expect("the document is never closed");
        Ok(json!({"type": "doc", "content": root.children}))
    }

    fn top(&mut self) -> &mut Frame<'k> {
        self.stack.last_mut().expect("the document is never closed")
    }

    /// The field whose rich text is being read: the innermost block's, or the root.
    fn kind(&self) -> &'k FieldKind {
        self.stack
            .iter()
            .rev()
            .find_map(|f| match f.open {
                Open::Body { text, .. } => Some(text.map_or(self.root, |f| &f.kind)),
                _ => None,
            })
            .unwrap_or(self.root)
    }

    /// Refuses a node at the next depth, past the limits, before reading any further.
    fn room(&mut self, line: usize) -> Result<(), Refusal> {
        if self.stack.len() > MAX_DEPTH {
            return Err((line, format!("nests more than {MAX_DEPTH} levels deep")));
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err((line, format!("holds more than {MAX_NODES} nodes")));
        }
        Ok(())
    }

    fn open(&mut self, line: usize, open: Open<'k>) -> Result<(), Refusal> {
        self.room(line)?;
        self.stack.push(Frame {
            line,
            open,
            children: Vec::new(),
        });
        Ok(())
    }

    fn push(&mut self, node: Map<String, Value>) -> Result<(), Refusal> {
        let line = node.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
        self.room(line)?;
        self.top().children.push(Value::Object(node));
        Ok(())
    }

    /// Adds a node built from a frame, already counted when it opened.
    fn finish(&mut self, node: Map<String, Value>) {
        self.top().children.push(Value::Object(node));
    }

    /// Opens the paragraph a tight list item or a table cell leaves unsaid.
    fn inline(&mut self, line: usize) -> Result<(), Refusal> {
        let blocks = match &self.top().open {
            Open::Doc | Open::Body { .. } | Open::Item(_) => true,
            Open::Node(n) => matches!(
                n["type"].as_str(),
                Some("blockquote" | "tableCell" | "tableHeader")
            ),
            _ => false,
        };
        if blocks {
            self.open(line, Open::Loose)?;
        }
        Ok(())
    }

    fn close_loose(&mut self) -> Result<(), Refusal> {
        if matches!(self.top().open, Open::Loose) {
            let frame = self.stack.pop().expect("just seen");
            self.paragraph(node("paragraph", frame.line), frame)?;
        }
        Ok(())
    }

    /// A paragraph or heading: the end of the text `<u>` can mark.
    fn paragraph(&mut self, mut n: Map<String, Value>, frame: Frame<'k>) -> Result<(), Refusal> {
        if !self.marks.is_empty() {
            return Err((frame.line, "`<u>` is never closed".into()));
        }
        if !frame.children.is_empty() {
            n.insert("content".into(), frame.children.into());
        }
        self.finish(n);
        Ok(())
    }

    fn event(&mut self, event: Event, line: usize) -> Result<(), Refusal> {
        match event {
            Event::Start(tag) => self.start(tag, line),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => self.text(&s, line, None),
            Event::Code(s) => self.text(&s, line, Some(json!({"type": "code"}))),
            Event::SoftBreak => self.text(" ", line, None),
            Event::HardBreak => {
                self.inline(line)?;
                self.push(node("hardBreak", line))
            }
            Event::Rule => {
                self.close_loose()?;
                self.push(node("horizontalRule", line))
            }
            Event::TaskListMarker(checked) => {
                if let Some(Open::Item(task)) = self
                    .stack
                    .iter_mut()
                    .rev()
                    .map(|f| &mut f.open)
                    .find(|o| matches!(o, Open::Item(_)))
                {
                    *task = Some(checked);
                }
                Ok(())
            }
            Event::InlineHtml(tag) => match &*tag {
                "<u>" => self.mark(json!({"type": "underline"}), line),
                "</u>" => self.unmark("underline", line),
                _ => Err((line, HTML.into())),
            },
            Event::Html(_) => Err((line, HTML.into())),
            // Footnotes, math and the like, which the options leave off.
            _ => Err((line, "this Markdown isn't supported".into())),
        }
    }

    /// Puts `mark` on the text that follows. One already on refuses as validation would,
    /// before nesting (`*_*_…`) can make every text node copy a long list.
    fn mark(&mut self, mark: Value, line: usize) -> Result<(), Refusal> {
        if let Some(t) = self.marks.iter().find(|m| m["type"] == mark["type"]) {
            return Err((
                line,
                format!("`{}` is applied twice", t["type"].as_str().unwrap_or("")),
            ));
        }
        self.inline(line)?;
        self.marks.push(mark);
        Ok(())
    }

    fn unmark(&mut self, t: &str, line: usize) -> Result<(), Refusal> {
        match self.marks.iter().rposition(|m| m["type"] == t) {
            Some(i) => {
                self.marks.remove(i);
                Ok(())
            }
            None => Err((line, "`</u>` closes nothing".into())),
        }
    }

    fn text(&mut self, s: &str, line: usize, code: Option<Value>) -> Result<(), Refusal> {
        if let Open::Code { text, .. } = &mut self.top().open {
            text.push_str(s);
            return Ok(());
        }
        if s.is_empty() {
            return Ok(());
        }
        self.inline(line)?;
        let mut marks = self.marks.clone();
        marks.extend(code);
        // In one order, whatever order the delimiters gave: `***both***` reads as either.
        marks.sort_by_key(|m| MARKS.iter().position(|t| m["type"] == *t));
        // Text the parser hands over in pieces, under the same marks, is one node.
        if let Some(Value::Object(last)) = self.top().children.last_mut()
            && last["type"] == "text"
            && last
                .get("marks")
                .map_or(marks.is_empty(), |m| *m == json!(marks))
            && let Some(Value::String(text)) = last.get_mut("text")
        {
            text.push_str(s);
            return Ok(());
        }
        let mut n = node("text", line);
        n.insert("text".into(), s.into());
        if !marks.is_empty() {
            n.insert("marks".into(), marks.into());
        }
        self.push(n)
    }

    fn start(&mut self, tag: Tag, line: usize) -> Result<(), Refusal> {
        let mark = match &tag {
            Tag::Emphasis => Some(json!({"type": "italic"})),
            Tag::Strong => Some(json!({"type": "bold"})),
            Tag::Strikethrough => Some(json!({"type": "strike"})),
            Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            } => {
                if !title.is_empty() {
                    return Err((
                        line,
                        "link titles aren't kept; leave out the quoted title".into(),
                    ));
                }
                Some(json!({"type": "link", "attrs": link(*link_type, dest_url, line)?}))
            }
            Tag::Image { .. } => {
                return Err((line, "images arrive with the media library".into()));
            }
            _ => None,
        };
        if let Some(mark) = mark {
            return self.mark(mark, line);
        }
        self.close_loose()?;
        let open = match tag {
            Tag::Paragraph => Open::Node(node("paragraph", line)),
            Tag::Heading { .. } if self.directives.contains_key(&line) => Open::Directive,
            Tag::Heading { level, .. } => {
                let mut n = node("heading", line);
                n.insert("attrs".into(), json!({"level": level as u8}));
                Open::Node(n)
            }
            Tag::BlockQuote(_) => Open::Node(node("blockquote", line)),
            Tag::CodeBlock(kind) => Open::Code {
                language: match kind {
                    CodeBlockKind::Fenced(info) if !info.trim().is_empty() => {
                        Some(info.trim().to_owned())
                    }
                    _ => None,
                },
                text: String::new(),
            },
            Tag::List(start) => Open::List {
                start,
                tasks: Vec::new(),
            },
            Tag::Item => Open::Item(None),
            Tag::Table(columns) => {
                if columns.iter().any(|a| *a != Alignment::None) {
                    return Err((
                        line,
                        "tables can't align columns; leave the colons out of the line under the header"
                            .into(),
                    ));
                }
                Open::Node(node("table", line))
            }
            Tag::TableHead => Open::TableRow { head: true },
            Tag::TableRow => Open::TableRow { head: false },
            Tag::TableCell => {
                let head = matches!(self.top().open, Open::TableRow { head: true });
                Open::Node(node(if head { "tableHeader" } else { "tableCell" }, line))
            }
            Tag::HtmlBlock => return Err((line, HTML.into())),
            _ => return Err((line, "this Markdown isn't supported".into())),
        };
        self.open(line, open)
    }

    fn end(&mut self, tag: TagEnd) -> Result<(), Refusal> {
        let line = self.stack.last().map_or(1, |f| f.line);
        match tag {
            TagEnd::Emphasis => return self.unmark("italic", line),
            TagEnd::Strong => return self.unmark("bold", line),
            TagEnd::Strikethrough => return self.unmark("strike", line),
            TagEnd::Link => return self.unmark("link", line),
            _ => {}
        }
        self.close_loose()?;
        let frame = self.stack.pop().expect("ends match starts");
        let (line, children) = (frame.line, frame.children);
        match frame.open {
            Open::Node(n) if matches!(n["type"].as_str(), Some("paragraph" | "heading")) => {
                return self.paragraph(
                    n,
                    Frame {
                        line,
                        open: Open::Doc,
                        children,
                    },
                );
            }
            Open::Node(mut n) => {
                n.insert("content".into(), filled(children, line).into());
                self.finish(n);
            }
            Open::Item(task) => {
                if let Open::List { tasks, .. } = &mut self.top().open {
                    tasks.push(task);
                }
                let mut n = node("listItem", line);
                n.insert("content".into(), filled(children, line).into());
                self.finish(n);
            }
            Open::List { start, tasks } => {
                let n = list(start, &tasks, children, line)?;
                self.finish(n);
            }
            Open::TableRow { .. } => {
                let mut n = node("tableRow", line);
                n.insert("content".into(), children.into());
                self.finish(n);
            }
            Open::Code { language, mut text } => {
                let mut n = node("codeBlock", line);
                if let Some(language) = language {
                    n.insert("attrs".into(), json!({ "language": language }));
                }
                if text.ends_with('\n') {
                    text.pop();
                }
                if !text.is_empty() {
                    n.insert("content".into(), json!([{"type": "text", "text": text}]));
                }
                self.finish(n);
            }
            Open::Directive => {
                self.nodes -= 1;
                self.directive(line)?;
            }
            Open::Body {
                colons, block_type, ..
            } => {
                return Err((
                    line,
                    format!("`{}{block_type}` is never closed", ":".repeat(colons)),
                ));
            }
            Open::Doc | Open::Loose => unreachable!("never ended by the parser"),
        }
        Ok(())
    }

    /// A directive's line: a block, a reference, or the end of a block's text.
    fn directive(&mut self, line: usize) -> Result<(), Refusal> {
        let at = self
            .directives
            .remove(&line)
            .expect("seen at the heading's start");
        let end = self.starts.get(line).copied().unwrap_or(self.src.len());
        let text = self.src[at..end].trim_end();
        let colons = text.len() - text.trim_start_matches(':').len();
        let rest = &text[colons..];
        let name_end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        let (name, attrs) = rest.split_at(name_end);
        let malformed = || {
            (
                line,
                format!("expected a directive like `::name{{key=\"value\"}}`, not `{text}`"),
            )
        };
        let attrs = match attrs {
            "" => Vec::new(),
            _ => attributes(attrs).ok_or_else(malformed)?,
        };
        if name.is_empty() {
            return match (colons, attrs.is_empty()) {
                (3.., true) => self.close_body(colons, line),
                _ => Err(malformed()),
            };
        }
        if name == "ref" {
            if colons != 2 {
                return Err((line, "`::ref` stands alone; it holds nothing".into()));
            }
            let get = |k: &str| attrs.iter().find(|(key, _)| key == k).map(|(_, v)| v);
            let (Some(to), Some(slug), 2) = (get("collection"), get("slug"), attrs.len()) else {
                return Err((line, "`::ref` takes a collection and a slug".into()));
            };
            let mut n = node("relationship", line);
            n.insert(
                "attrs".into(),
                json!({"relationTo": to, "slug": slug, "line": line}),
            );
            return self.push(n);
        }
        let Some(block) = self.kind().block(name) else {
            return Err((
                line,
                format!("unknown directive `{}{name}`", ":".repeat(colons)),
            ));
        };
        let mut fields = Map::new();
        for (key, value) in attrs {
            let field = block.fields.iter().find(|f| f.name == key);
            let value = match field {
                // Written as JSON whatever it holds, so a string reads back as a string.
                Some(f) if matches!(f.kind, FieldKind::Json) => {
                    match serde_json::from_str(&value) {
                        Ok(v) => v,
                        Err(_) => {
                            return Err((
                                line,
                                format!(
                                    "`{key}` holds JSON, so its value must be JSON, like `\\\"text\\\"` or `30`"
                                ),
                            ));
                        }
                    }
                }
                Some(f) if !takes_text(&f.kind) => {
                    serde_json::from_str(&value).unwrap_or(Value::String(value))
                }
                _ => Value::String(value),
            };
            fields.insert(key, value);
        }
        if colons == 2 {
            let mut n = node("block", line);
            n.insert("attrs".into(), block_attrs(name, fields));
            return self.push(n);
        }
        let texts: Vec<&Field> = block
            .fields
            .iter()
            .filter(|f| matches!(f.kind, FieldKind::RichText { .. }))
            .collect();
        let text = match texts[..] {
            [f] => Some(f),
            _ => None,
        };
        if let Some(f) = text.filter(|f| fields.contains_key(&f.name)) {
            return Err((
                line,
                format!(
                    "`{}` is the text inside `{}{name}`; it can't be an attribute too",
                    f.name,
                    ":".repeat(colons)
                ),
            ));
        }
        self.open(
            line,
            Open::Body {
                colons,
                block_type: name.to_owned(),
                fields,
                text,
            },
        )
    }

    /// The line closing the innermost block's text, which has as many colons as it opened
    /// with, so a nested block's closing line can't be taken for its outer one's.
    fn close_body(&mut self, colons: usize, line: usize) -> Result<(), Refusal> {
        let shut = ":".repeat(colons);
        let frame = self.stack.pop().expect("the document is never closed");
        let Frame {
            line: opened,
            open:
                Open::Body {
                    colons: wanted,
                    block_type,
                    mut fields,
                    text,
                },
            children,
        } = frame
        else {
            return Err((line, format!("`{shut}` closes nothing")));
        };
        if colons != wanted {
            return Err((
                line,
                format!(
                    "`{shut}` doesn't close `{}{block_type}` from line {opened}",
                    ":".repeat(wanted)
                ),
            ));
        }
        match text {
            Some(f) => {
                fields.insert(f.name.clone(), json!({"type": "doc", "content": children}));
            }
            None if !children.is_empty() => {
                return Err((
                    opened,
                    format!(
                        "`{block_type}` has no single rich text field to hold text; write it as `::{block_type}{{…}}`"
                    ),
                ));
            }
            None => {}
        }
        let mut n = node("block", opened);
        n.insert("attrs".into(), block_attrs(&block_type, fields));
        self.finish(n);
        Ok(())
    }
}

/// A link mark's attributes: `doc:<collection>/<slug>` names a document by slug, as
/// `to_markdown` writes it; anything else is an `href`, checked by validation.
fn link(link_type: LinkType, dest: &str, line: usize) -> Result<Value, Refusal> {
    if let Some(rest) = dest.strip_prefix("doc:") {
        let Some((to, slug)) = rest
            .split_once('/')
            .filter(|(to, slug)| !to.is_empty() && !slug.is_empty())
        else {
            return Err((
                line,
                format!("expected doc:<collection>/<slug>, not `{dest}`"),
            ));
        };
        return Ok(json!({"doc": {"relationTo": to, "slug": slug, "line": line}}));
    }
    Ok(match link_type {
        LinkType::Email => json!({ "href": format!("mailto:{dest}") }),
        _ => json!({ "href": dest }),
    })
}

fn block_attrs(block_type: &str, fields: Map<String, Value>) -> Value {
    match fields.is_empty() {
        true => json!({ "blockType": block_type }),
        false => json!({"blockType": block_type, "fields": fields}),
    }
}

/// What a node holding blocks needs at least: an empty paragraph, if nothing else.
fn filled(mut children: Vec<Value>, line: usize) -> Vec<Value> {
    if children.is_empty() {
        children.push(node("paragraph", line).into());
    }
    children
}

/// A list's node, by its start and its items' task markers.
fn list(
    start: Option<u64>,
    tasks: &[Option<bool>],
    mut items: Vec<Value>,
    line: usize,
) -> Result<Map<String, Value>, Refusal> {
    let marked = tasks.iter().filter(|t| t.is_some()).count();
    let mut n = match (marked, start) {
        (0, None) => node("bulletList", line),
        (0, Some(start)) => {
            let mut n = node("orderedList", line);
            if start != 1 {
                n.insert("attrs".into(), json!({ "start": start }));
            }
            n
        }
        (_, Some(_)) => {
            return Err((
                line,
                "a task list can't be numbered; start its items with `- [ ]`".into(),
            ));
        }
        (m, None) if m < tasks.len() => {
            return Err((line, "a list can't mix task items with plain ones".into()));
        }
        _ => {
            for (item, checked) in items.iter_mut().zip(tasks) {
                item["type"] = "taskItem".into();
                item["attrs"] = json!({ "checked": checked });
            }
            node("taskList", line)
        }
    };
    n.insert("content".into(), items.into());
    Ok(n)
}

/// Where a line's directive starts, past any quote and list markers: the first of at
/// least two colons.
fn directive_at(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let skip = |mut i: usize| {
        while b.get(i).is_some_and(|c| b" \t>".contains(c)) {
            i += 1;
        }
        i
    };
    let mut i = skip(0);
    // A list item's marker: `-`, `+` or `*`, or digits and `.` or `)`; then a space.
    let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let marker = match b.get(i) {
        Some(b'-' | b'+' | b'*') => 1,
        _ if digits > 0 && b.get(i + digits).is_some_and(|c| b".)".contains(c)) => digits + 1,
        _ => 0,
    };
    if marker > 0 && b.get(i + marker).is_some_and(|c| b" \t".contains(c)) {
        i = skip(i + marker);
    }
    b[i..].starts_with(b"::").then_some(i)
}

/// `{key="value" …}`: values double-quoted, with `\\`, `\"`, `\n` and `\r` escaped, as
/// `to_markdown` writes them. `None` for anything else, or a key given twice.
fn attributes(s: &str) -> Option<Vec<(String, String)>> {
    let mut chars = s.strip_prefix('{')?.chars().peekable();
    let mut out: Vec<(String, String)> = Vec::new();
    loop {
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        match chars.next()? {
            '}' => return chars.next().is_none().then_some(out),
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let mut key = String::from(c);
                while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
                    key.push(c);
                }
                if chars.next()? != '=' || chars.next()? != '"' {
                    return None;
                }
                let mut value = String::new();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => value.push(match chars.next()? {
                            '\\' => '\\',
                            '"' => '"',
                            'n' => '\n',
                            'r' => '\r',
                            _ => return None,
                        }),
                        c => value.push(c),
                    }
                }
                if out.iter().any(|(k, _)| *k == key) {
                    return None;
                }
                out.push((key, value));
            }
            _ => return None,
        }
    }
}

/// Whether a field of `kind` stores a string, so its attribute is kept as written; any
/// other is read as the JSON `to_markdown` wrote (`30`, `true`, `{…}`).
fn takes_text(kind: &FieldKind) -> bool {
    matches!(value_kind(kind), Some(ValueKind::Text | ValueKind::Date))
        && !matches!(kind, FieldKind::Relationship { relation_to, .. } if relation_to.is_polymorphic())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{A1, P1};
    use super::super::{RenderCtx, to_markdown};
    use super::*;
    use crate::schema::Field;
    use serde_json::json;

    /// A `body` field linking to posts and authors, with a `callout` block holding text
    /// (which may hold an `aside`, which holds text in turn) and a `video` block.
    fn body() -> Field {
        serde_json::from_value(json!({"name": "body", "type": "richText",
            "relationTo": ["posts", "authors"], "blocks": [
            {"slug": "callout", "fields": [
                {"name": "kind", "type": "text"},
                {"name": "body", "type": "richText", "relationTo": ["posts"], "blocks": [
                    {"slug": "aside", "fields": [{"name": "text", "type": "richText"}]}
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

    fn convert(src: &str) -> Value {
        match from_markdown(src, &body().kind) {
            Ok(doc) => doc,
            Err(e) => panic!("{src}: {e:?}"),
        }
    }

    fn t(s: &str) -> Value {
        json!({"type": "text", "text": s})
    }

    fn marked(s: &str, marks: Value) -> Value {
        json!({"type": "text", "text": s, "marks": marks})
    }

    fn code(s: &str) -> Value {
        marked(s, json!([{"type": "code"}]))
    }

    fn para(content: Value) -> Value {
        json!({"type": "paragraph", "content": content})
    }

    fn heading(level: u8, s: &str) -> Value {
        json!({"type": "heading", "attrs": {"level": level}, "content": [t(s)]})
    }

    fn item(kind: &str, content: Value) -> Value {
        json!({"type": kind, "content": content})
    }

    fn task(checked: bool, content: Value) -> Value {
        json!({"type": "taskItem", "attrs": {"checked": checked}, "content": content})
    }

    fn doc(content: Value) -> Value {
        json!({"type": "doc", "content": content})
    }

    #[test]
    fn markdown_converts_real_posts() {
        let cell = |kind: &str, content: Value| json!({"type": kind, "content": [para(content)]});
        let guide = concat!(
            "# Getting started\n",
            "\n",
            "Install the **CLI** with `cargo`, then read [the docs](https://example.com/docs)\n",
            "or [the intro](doc:posts/intro).\n",
            "\n",
            "```sh\n",
            "cargo install tachyon\n",
            "```\n",
            "\n",
            "| Command | What it does |\n",
            "| --- | --- |\n",
            "| `migrate` | Applies *pending* migrations |\n",
            "| `serve` | Starts the server |\n",
            "\n",
            "1. Run it.\n",
            "2. Check the log:\n",
            "\n",
            "   ```text\n",
            "   ready on :3000\n",
            "   ```\n",
        );
        assert_eq!(
            convert(guide),
            doc(json!([
                heading(1, "Getting started"),
                para(json!([
                    t("Install the "),
                    marked("CLI", json!([{"type": "bold"}])),
                    t(" with "),
                    code("cargo"),
                    t(", then read "),
                    marked("the docs", json!([{"type": "link", "attrs": {"href": "https://example.com/docs"}}])),
                    t(" or "),
                    marked("the intro", json!([{"type": "link", "attrs": {
                        "doc": {"relationTo": "posts", "slug": "intro", "line": 4}
                    }}])),
                    t(".")
                ])),
                {"type": "codeBlock", "attrs": {"language": "sh"}, "content": [t("cargo install tachyon")]},
                {"type": "table", "content": [
                    {"type": "tableRow", "content": [
                        cell("tableHeader", json!([t("Command")])),
                        cell("tableHeader", json!([t("What it does")]))
                    ]},
                    {"type": "tableRow", "content": [
                        cell("tableCell", json!([code("migrate")])),
                        cell("tableCell", json!([
                            t("Applies "), marked("pending", json!([{"type": "italic"}])), t(" migrations")
                        ]))
                    ]},
                    {"type": "tableRow", "content": [
                        cell("tableCell", json!([code("serve")])),
                        cell("tableCell", json!([t("Starts the server")]))
                    ]}
                ]},
                {"type": "orderedList", "content": [
                    item("listItem", json!([para(json!([t("Run it.")]))])),
                    item("listItem", json!([
                        para(json!([t("Check the log:")])),
                        {"type": "codeBlock", "attrs": {"language": "text"}, "content": [t("ready on :3000")]}
                    ]))
                ]}
            ]))
        );

        let page = concat!(
            "## Why it exists\n",
            "\n",
            "> Writing should feel like *writing*.\n",
            "\n",
            ":::callout{kind=\"warning\"}\n",
            "Back up first. See [the guide](doc:posts/backups).\n",
            "\n",
            "- one\n",
            "- two\n",
            ":::\n",
            "\n",
            "::video{url=\"https://example.com/v.mp4\" start=\"30\" autoplay=\"true\"}\n",
            "\n",
            "Watch it <u>twice</u>, then ~~skip~~ read on:\n",
            "\n",
            "::ref{collection=\"authors\" slug=\"ann\"}\n",
            "\n",
            "---\n",
        );
        assert_eq!(
            convert(page),
            doc(json!([
                heading(2, "Why it exists"),
                {"type": "blockquote", "content": [para(json!([
                    t("Writing should feel like "), marked("writing", json!([{"type": "italic"}])), t(".")
                ]))]},
                {"type": "block", "attrs": {"blockType": "callout", "fields": {
                    "kind": "warning",
                    "body": doc(json!([
                        para(json!([
                            t("Back up first. See "),
                            marked("the guide", json!([{"type": "link", "attrs": {
                                "doc": {"relationTo": "posts", "slug": "backups", "line": 6}
                            }}])),
                            t(".")
                        ])),
                        {"type": "bulletList", "content": [
                            item("listItem", json!([para(json!([t("one")]))])),
                            item("listItem", json!([para(json!([t("two")]))]))
                        ]}
                    ]))
                }}},
                {"type": "block", "attrs": {"blockType": "video", "fields": {
                    "url": "https://example.com/v.mp4", "start": 30, "autoplay": true
                }}},
                para(json!([
                    t("Watch it "),
                    marked("twice", json!([{"type": "underline"}])),
                    t(", then "),
                    marked("skip", json!([{"type": "strike"}])),
                    t(" read on:")
                ])),
                {"type": "relationship", "attrs": {"relationTo": "authors", "slug": "ann", "line": 16}},
                {"type": "horizontalRule"}
            ]))
        );

        let changelog = concat!(
            "## v0.2.0\n",
            "\n",
            "- [x] Rich text\n",
            "- [ ] Media library\n",
            "  - [ ] Image resizing\n",
            "\n",
            "### Fixed\n",
            "\n",
            "* Links to `mailto:` addresses work\\\n",
            "  again.\n",
            "* Tables keep their header row.\n",
        );
        assert_eq!(
            convert(changelog),
            doc(json!([
                heading(2, "v0.2.0"),
                {"type": "taskList", "content": [
                    task(true, json!([para(json!([t("Rich text")]))])),
                    task(false, json!([
                        para(json!([t("Media library")])),
                        {"type": "taskList", "content": [
                            task(false, json!([para(json!([t("Image resizing")]))]))
                        ]}
                    ]))
                ]},
                heading(3, "Fixed"),
                {"type": "bulletList", "content": [
                    item("listItem", json!([para(json!([
                        t("Links to "), code("mailto:"), t(" addresses work"),
                        {"type": "hardBreak"}, t("again.")
                    ]))])),
                    item("listItem", json!([para(json!([t("Tables keep their header row.")]))]))
                ]}
            ]))
        );
    }

    #[test]
    fn markdown_refuses_with_line_numbers() {
        let html = "raw HTML isn't allowed; underline is the only tag, as <u>…</u>";
        let scheme = "links must start with http://, https://, mailto:, tel:, / or #";
        let bold_only: Field = serde_json::from_value(
            json!({"name": "body", "type": "richText", "features": ["bold"]}),
        )
        .unwrap();
        let b = body();
        let deep_quote = format!("{} deep", ">".repeat(40));
        let deep_list = format!("{}deep", "- ".repeat(40));
        let rules = "---\n".repeat(50_001);
        let huge_quote = ">".repeat(1_000_000);
        let cases: Vec<(&Field, &str, &str, String)> = vec![
            (&b, "Hello\n\n<div>hi</div>", "", format!("line 3: {html}")),
            (&b, "a <span>b</span>", "", format!("line 1: {html}")),
            (&b, "<u>open", "", "line 1: `<u>` is never closed".into()),
            (&b, "shut</u>", "", "line 1: `</u>` closes nothing".into()),
            (&b, "<u>a <u>b</u></u>", "", "line 1: `underline` is applied twice".into()),
            (&b, "*a _b *c* d_ e*", "", "line 1: `italic` is applied twice".into()),
            (
                &b,
                "text\n\n![cat](/cat.png)",
                "",
                "line 3: images arrive with the media library".into(),
            ),
            (
                &b,
                "::video{url=nope}",
                "",
                "line 1: expected a directive like `::name{key=\"value\"}`, not `::video{url=nope}`"
                    .into(),
            ),
            (
                &b,
                "::video{url=\"a\" url=\"b\"}",
                "",
                "line 1: expected a directive like `::name{key=\"value\"}`, not `::video{url=\"a\" url=\"b\"}`"
                    .into(),
            ),
            (&b, "::embed{}", "", "line 1: unknown directive `::embed`".into()),
            (
                &b,
                "intro\n\n:::callout\ntext",
                "",
                "line 3: `:::callout` is never closed".into(),
            ),
            (
                &b,
                "- item\n  :::callout\n  inside\n\nafter",
                "",
                "line 2: `:::callout` is never closed".into(),
            ),
            (&b, "a\n\n:::", "", "line 3: `:::` closes nothing".into()),
            (
                &b,
                "::::callout\n:::aside\nx\n::::",
                "",
                "line 4: `::::` doesn't close `:::aside` from line 2".into(),
            ),
            (
                &b,
                ":::ref{collection=\"posts\" slug=\"a\"}\n:::",
                "",
                "line 1: `::ref` stands alone; it holds nothing".into(),
            ),
            (
                &b,
                "::ref{collection=\"posts\"}",
                "",
                "line 1: `::ref` takes a collection and a slug".into(),
            ),
            (
                &b,
                ":::video{url=\"x\"}\ntext\n:::",
                "",
                "line 1: `video` has no single rich text field to hold text; write it as `::video{…}`"
                    .into(),
            ),
            (
                &b,
                ":::callout{body=\"x\"}\n:::",
                "",
                "line 1: `body` is the text inside `:::callout`; it can't be an attribute too".into(),
            ),
            (
                &b,
                "text\n    ::video{url=\"x\"}",
                "",
                "line 2: a directive must start its own line, indented less than four spaces"
                    .into(),
            ),
            (
                &b,
                "[x](doc:posts)",
                "",
                "line 1: expected doc:<collection>/<slug>, not `doc:posts`".into(),
            ),
            (
                &b,
                "[x](/a \"Title\")",
                "",
                "line 1: link titles aren't kept; leave out the quoted title".into(),
            ),
            (
                &b,
                "| a | b |\n|:--|---|\n| 1 | 2 |",
                "",
                "line 1: tables can't align columns; leave the colons out of the line under the header"
                    .into(),
            ),
            (
                &b,
                "- [ ] a\n- b",
                "",
                "line 1: a list can't mix task items with plain ones".into(),
            ),
            (
                &b,
                "1. [ ] a",
                "",
                "line 1: a task list can't be numbered; start its items with `- [ ]`".into(),
            ),
            (&b, &deep_quote, "", "line 1: nests more than 32 levels deep".into()),
            (&b, &deep_list, "", "line 1: nests more than 32 levels deep".into()),
            (&b, &huge_quote, "", "line 1: nests more than 32 levels deep".into()),
            (&b, &rules, "", "line 50001: holds more than 50000 nodes".into()),
            // A lone `\r` ends a line, as in CommonMark.
            (
                &b,
                "a\r\r[x](ftp://a)",
                "content.1.content.0.marks.0.attrs.href",
                format!("line 3: {scheme}"),
            ),
            (&b, "a\r::embed{}", "", "line 2: unknown directive `::embed`".into()),
            (
                &b,
                "::video{url=\"x\" meta=\"{oops\"}",
                "",
                "line 1: `meta` holds JSON, so its value must be JSON, like `\\\"text\\\"` or `30`"
                    .into(),
            ),
            // The validator's refusals, at the node, on its line.
            (
                &bold_only,
                "Fine\n\n## Heading",
                "content.1",
                "line 3: the `heading` feature isn't turned on for this field".into(),
            ),
            (
                &bold_only,
                "Fine *really*",
                "content.0.content.1.marks.0",
                "line 1: the `italic` feature isn't turned on for this field".into(),
            ),
            (
                &b,
                "one\n\n[x](javascript:alert(1))",
                "content.1.content.0.marks.0.attrs.href",
                format!("line 3: {scheme}"),
            ),
            (
                &b,
                "::video{start=\"3\"}",
                "content.0.attrs.fields.url",
                "line 1: is required".into(),
            ),
            (
                &b,
                "::video{url=\"x\" start=\"soon\"}",
                "content.0.attrs.fields.start",
                "line 1: expected a number".into(),
            ),
            (
                &b,
                "[x](doc:pages/home)",
                "content.0.content.0.marks.0.attrs.doc.relationTo",
                "line 1: can't link to `pages`; expected one of: posts, authors".into(),
            ),
            // Inside a block's text: its own field's rules, its own lines.
            (
                &b,
                ":::callout\nfine\n\n::video{url=\"x\"}\n:::",
                "",
                "line 4: unknown directive `::video`".into(),
            ),
            (
                &b,
                ":::callout\nsee [x](ftp://a)\n:::",
                "content.0.attrs.fields.body.content.0.content.1.marks.0.attrs.href",
                format!("line 2: {scheme}"),
            ),
            (
                &b,
                ":::callout\n[x](doc:authors/ann)\n:::",
                "content.0.attrs.fields.body.content.0.content.0.marks.0.attrs.doc.relationTo",
                "line 2: can't link to `authors`; expected one of: posts".into(),
            ),
        ];
        for (field, src, path, message) in cases {
            let shown = src.chars().take(60).collect::<String>();
            assert_eq!(
                from_markdown(src, &field.kind),
                Err(vec![FieldError {
                    path: path.into(),
                    message
                }]),
                "{shown}"
            );
        }
    }

    /// Each document named by slug, by its ID: as the API finds them.
    fn by_id(mut doc: Value) -> Value {
        slug_refs_mut(&mut doc, &mut |_, link| {
            let id = match link["slug"].as_str() {
                Some("intro") => P1,
                Some("ann") => A1,
                other => panic!("{other:?}"),
            };
            link.remove("slug");
            link.remove("line");
            link.insert("value".into(), id.into());
        });
        doc
    }

    #[test]
    fn markdown_round_trips() {
        let none = |_: &str, _: &Value| None;
        let slug = |to: &str, v: &Value| match (to, v.as_str()?) {
            ("posts", P1) => Some("intro".to_owned()),
            ("authors", A1) => Some("ann".to_owned()),
            _ => None,
        };
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &slug,
        };
        let link = |s: &str, attrs: Value| marked(s, json!([{"type": "link", "attrs": attrs}]));
        let video = json!({"type": "block", "attrs": {"blockType": "video", "fields": {
            "url": "https://example.com/v.mp4", "start": 30, "autoplay": false
        }}});
        let callout = |kind: &str, body: Value| json!({"type": "block", "attrs": {"blockType": "callout", "fields": {"kind": kind, "body": body}}});
        let aside = json!({"type": "block", "attrs": {"blockType": "aside", "fields": {
            "text": doc(json!([para(json!([t("aside")]))]))
        }}});
        let cell = |kind: &str, content: Value| json!({"type": kind, "content": [para(content)]});
        let fixtures = [
            doc(json!([])),
            // Text that looks like Markdown stays text.
            doc(json!([
                para(json!([t("# not *a* [heading] <b> | & \\ ~x~ `y` _z_")])),
                para(json!([t("1. not a list")])),
                para(json!([t(":::not a directive")])),
                para(json!([t("- not a list either")])),
                para(json!([t("Wow!"), link("x", json!({"href": "/a"}))])),
            ])),
            // Marks, alone and together.
            doc(json!([para(json!([
                marked("b", json!([{"type": "bold"}])),
                t(" "),
                marked("i", json!([{"type": "italic"}])),
                t(" "),
                marked("both", json!([{"type": "bold"}, {"type": "italic"}])),
                t(" "),
                marked("gone", json!([{"type": "strike"}])),
                t(" "),
                marked("under", json!([{"type": "underline"}])),
                t(" "),
                code("a`b"),
                t(" "),
                link("(p)", json!({"href": "/a_(b)"})),
                t(" "),
                link("intro", json!({"doc": {"relationTo": "posts", "value": P1}})),
                t(" "),
                marked("bold link", json!([
                    {"type": "bold"}, {"type": "link", "attrs": {"href": "https://example.com/?q=1&r=2"}}
                ])),
                {"type": "hardBreak"},
                t("after a break")
            ]))])),
            // Marks that touch.
            doc(json!([para(json!([
                marked("b", json!([{"type": "bold"}])),
                marked("i", json!([{"type": "italic"}])),
                marked("s", json!([{"type": "strike"}])),
                code("c")
            ]))])),
            // Structure.
            doc(json!([
                heading(1, "One"),
                heading(3, "Three"),
                {"type": "bulletList", "content": [
                    item("listItem", json!([
                        para(json!([t("one")])),
                        {"type": "orderedList", "attrs": {"start": 3}, "content": [
                            item("listItem", json!([para(json!([t("three")]))])),
                            item("listItem", json!([para(json!([t("four")]))]))
                        ]}
                    ])),
                    item("listItem", json!([para(json!([t("two")])), para(json!([t("more")]))]))
                ]},
                {"type": "orderedList", "content": [item("listItem", json!([para(json!([t("first")]))]))]},
                {"type": "taskList", "content": [
                    task(true, json!([para(json!([t("done")]))])),
                    task(false, json!([
                        para(json!([t("to do")])),
                        {"type": "bulletList", "content": [item("listItem", json!([para(json!([t("sub")]))]))]}
                    ]))
                ]},
                {"type": "blockquote", "content": [
                    para(json!([t("quoted")])),
                    {"type": "bulletList", "content": [item("listItem", json!([para(json!([t("in a quote")]))]))]},
                    {"type": "blockquote", "content": [para(json!([t("deeper")]))]}
                ]},
                {"type": "horizontalRule"},
                {"type": "codeBlock", "attrs": {"language": "c++"}, "content": [t("a\n```\n:::callout\n::video{}\nb")]},
                {"type": "codeBlock"},
                {"type": "table", "content": [
                    {"type": "tableRow", "content": [cell("tableHeader", json!([t("a")])), cell("tableHeader", json!([t("b")]))]},
                    {"type": "tableRow", "content": [
                        cell("tableCell", json!([marked("x", json!([{"type": "bold"}]))])),
                        {"type": "tableCell", "content": [{"type": "paragraph"}]}
                    ]}
                ]}
            ])),
            // Blocks and references, at the top and inside other nodes.
            doc(json!([
                video.clone(),
                {"type": "relationship", "attrs": {"relationTo": "authors", "value": A1}},
                callout("say \"hi\"\r\nback\\slash", doc(json!([
                    para(json!([t("inside")])),
                    aside,
                    para(json!([link("intro", json!({"doc": {"relationTo": "posts", "value": P1}}))]))
                ]))),
                callout("empty", doc(json!([]))),
                {"type": "bulletList", "content": [item("listItem", json!([
                    para(json!([t("item")])),
                    callout("listed", doc(json!([para(json!([t("in a list")]))])))
                ]))]},
                {"type": "blockquote", "content": [video]}
            ])),
            // Line breaks inside text stay text, wherever the text is (W1-R28).
            doc(json!([
                para(json!([t(BROKEN)])),
                para(json!([t("\n"), marked(BROKEN, json!([{"type": "bold"}])), t("\r")])),
                para(json!([t("a"), {"type": "hardBreak"}, t("# b"), {"type": "hardBreak"}, t("::video{}")])),
                heading(2, BROKEN),
                {"type": "table", "content": [
                    {"type": "tableRow", "content": [cell("tableHeader", json!([t(BROKEN)]))]},
                    {"type": "tableRow", "content": [cell("tableCell", json!([t(BROKEN)]))]}
                ]}
            ])),
            // A json field keeps its value's type, a string that reads as JSON included.
            doc(json!([
                meta(json!("30")),
                meta(json!("true")),
                meta(json!(30)),
                meta(json!({"a": [1, "x"]})),
                // Shaped like rich text, but a json field's, so it's an attribute.
                meta(json!({"type": "doc", "content": []}))
            ])),
        ];
        let kind = body().kind;
        for fixture in fixtures {
            let markdown = to_markdown(&fixture, &kind, &ctx);
            let back = from_markdown(&markdown, &kind).map(by_id);
            assert_eq!(back, Ok(fixture), "{markdown}");
        }

        // Inline code can't hold a line break, so the code mark is split around it.
        let split = doc(json!([para(json!([code("a\nb\r\nc")]))]));
        let markdown = to_markdown(&split, &kind, &ctx);
        assert_eq!(markdown, "`a`&#10;`b`&#13;&#10;`c`");
        assert_eq!(
            from_markdown(&markdown, &kind),
            Ok(doc(json!([para(json!([
                code("a"),
                t("\n"),
                code("b"),
                t("\r\n"),
                code("c")
            ]))])))
        );
        // Spaces at a split span's ends stay inside it, padded so Markdown keeps them.
        let spaced = doc(json!([para(json!([code("a\n b \nc")]))]));
        let markdown = to_markdown(&spaced, &kind, &ctx);
        assert_eq!(markdown, "`a`&#10;`  b  `&#10;`c`");
        assert_eq!(
            from_markdown(&markdown, &kind),
            Ok(doc(json!([para(json!([
                code("a"),
                t("\n"),
                code(" b "),
                t("\n"),
                code("c")
            ]))])))
        );
    }

    /// Spaces and tabs at a line's end stay text, and never make a hard break (W1-R31).
    #[test]
    fn markdown_line_ends_keep_their_spaces() {
        let none = |_: &str, _: &Value| None;
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &none,
        };
        let kind = body().kind;
        for end in [" ", "  ", "   ", "\t", " \t"] {
            let line = format!("b{end}");
            let cell = |kind: &str| json!({"type": kind, "content": [para(json!([t(&line)]))]});
            let fixture = doc(json!([
                para(json!([t(&line)])),
                para(json!([t(&line), {"type": "hardBreak"}, t("c")])),
                para(json!([t(&format!("{line}\nc"))])),
                para(json!([t(&format!("{line}\r\nc"))])),
                // Across nodes: the run and the break written by different text nodes.
                para(json!([marked(&line, json!([{"type": "bold"}])), t("\nc")])),
                para(json!([t(&line), marked("\nc", json!([{"type": "italic"}]))])),
                heading(2, &line),
                {"type": "table", "content": [
                    {"type": "tableRow", "content": [cell("tableHeader")]},
                    {"type": "tableRow", "content": [cell("tableCell")]}
                ]}
            ]));
            let markdown = to_markdown(&fixture, &kind, &ctx);
            assert_eq!(
                from_markdown(&markdown, &kind),
                Ok(fixture),
                "{end:?}\n{markdown}"
            );
        }
    }

    /// Spaces and tabs at a marked text's edges stay inside its mark (W1-R32).
    #[test]
    fn markdown_mark_edges_stay_inside() {
        let none = |_: &str, _: &Value| None;
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &none,
        };
        let kind = body().kind;
        let marks = [
            json!([{"type": "bold"}]),
            json!([{"type": "italic"}]),
            json!([{"type": "strike"}]),
            json!([{"type": "underline"}]),
            json!([{"type": "link", "attrs": {"href": "/a"}}]),
            json!([{"type": "bold"}, {"type": "italic"}]),
        ];
        // The parser reads entity spaces inside delimiters as the mark's own text.
        assert_eq!(
            from_markdown("x **&#32;b&#9;** *&#32;*", &kind),
            Ok(doc(json!([para(json!([
                t("x "),
                marked(" b\t", json!([{"type": "bold"}])),
                t(" "),
                marked(" ", json!([{"type": "italic"}]))
            ]))])))
        );
        for m in marks {
            // A neighbour under other marks, or the two would read back as one node.
            let other = match m[0]["type"] == "strike" {
                true => json!([{"type": "underline"}]),
                false => json!([{"type": "strike"}]),
            };
            let fixture = doc(json!([
                para(json!([t("x"), marked(" b ", m.clone()), t("y")])),
                para(json!([marked("\tb\t", m.clone())])),
                para(json!([
                    marked("  b", m.clone()),
                    marked("c  ", other.clone())
                ])),
                para(json!([t("x"), marked(" ", m.clone()), t("y")])),
                para(json!([marked(" \t ", m.clone())])),
                // Punctuation inside a delimiter beside letters outside it.
                para(json!([t("x"), marked("&b!", m.clone()), t("y")])),
                para(json!([
                    t("1"),
                    marked("\u{2014}b\u{2014}", m.clone()),
                    t("2")
                ])),
            ]));
            let markdown = to_markdown(&fixture, &kind, &ctx);
            assert_eq!(
                from_markdown(&markdown, &kind),
                Ok(fixture),
                "{m}\n{markdown}"
            );
        }
    }

    /// Text nodes whose marks overlap, touch or differ read back as written (W1-R33).
    #[test]
    fn markdown_touching_marks_round_trip() {
        let none = |_: &str, _: &Value| None;
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &none,
        };
        let kind = body().kind;
        let sets = [
            json!([]),
            json!([{"type": "bold"}]),
            json!([{"type": "italic"}]),
            json!([{"type": "strike"}]),
            json!([{"type": "underline"}]),
            json!([{"type": "code"}]),
            json!([{"type": "link", "attrs": {"href": "/a"}}]),
            json!([{"type": "bold"}, {"type": "italic"}]),
        ];
        let node = |text: &str, marks: &Value| match marks.as_array().unwrap().is_empty() {
            true => t(text),
            false => marked(text, marks.clone()),
        };
        // Adjacent text under the same marks reads back as one node.
        let merged = |nodes: Vec<Value>| {
            let mut out: Vec<Value> = Vec::new();
            for n in nodes {
                match out.last_mut() {
                    Some(last) if last.get("marks") == n.get("marks") => {
                        let joined = format!(
                            "{}{}",
                            last["text"].as_str().unwrap(),
                            n["text"].as_str().unwrap()
                        );
                        last["text"] = joined.into();
                    }
                    _ => out.push(n),
                }
            }
            para(Value::Array(out))
        };
        let joins = ["x", " ", "!"];
        for a in &sets {
            for b in &sets {
                let (mut fixture, mut expected) = (Vec::new(), Vec::new());
                for (ja, jb) in joins
                    .iter()
                    .flat_map(|ja| joins.iter().map(move |jb| (ja, jb)))
                {
                    let pair = vec![node(&format!("a{ja}"), a), node(&format!("{jb}b"), b)];
                    let around = vec![t("p"), pair[0].clone(), pair[1].clone(), t("q")];
                    fixture.push(para(Value::Array(pair.clone())));
                    expected.push(merged(pair));
                    fixture.push(para(Value::Array(around.clone())));
                    expected.push(merged(around));
                }
                let markdown = to_markdown(&doc(Value::Array(fixture)), &kind, &ctx);
                assert_eq!(
                    from_markdown(&markdown, &kind),
                    Ok(doc(Value::Array(expected))),
                    "{a} then {b}\n{markdown}"
                );
            }
        }
        // Three in a row, so a mark kept open across nodes nests with the others.
        for a in &sets {
            for b in &sets {
                let fixture: Vec<Value> = sets
                    .iter()
                    .map(|c| para(json!([node("a", a), node(" b!", b), node("c", c)])))
                    .collect();
                let expected: Vec<Value> = sets
                    .iter()
                    .map(|c| merged(vec![node("a", a), node(" b!", b), node("c", c)]))
                    .collect();
                let markdown = to_markdown(&doc(Value::Array(fixture)), &kind, &ctx);
                assert_eq!(
                    from_markdown(&markdown, &kind),
                    Ok(doc(Value::Array(expected))),
                    "{a} then {b}\n{markdown}"
                );
            }
        }
    }

    /// Links side by side, and marks around a hard break, read back as written.
    #[test]
    fn markdown_links_and_breaks_round_trip() {
        let none = |_: &str, _: &Value| None;
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &none,
        };
        let kind = body().kind;
        let link = |href: &str| json!({"type": "link", "attrs": {"href": href}});
        let with = |marks: Vec<Value>| Value::Array(marks);
        let extras = [
            vec![],
            vec![json!({"type": "bold"})],
            vec![json!({"type": "italic"})],
            vec![json!({"type": "code"})],
        ];
        let joins = ["x", " ", "!"];
        let mut fixture = Vec::new();
        let mut expected = Vec::new();
        for ea in &extras {
            for eb in &extras {
                for ja in joins {
                    for jb in joins {
                        // Marks in the reader's order: bold, italic, link, code.
                        let marks = |e: &Vec<Value>, href: &str| {
                            let mut m: Vec<Value> =
                                e.iter().filter(|m| m["type"] != "code").cloned().collect();
                            m.push(link(href));
                            m.extend(e.iter().filter(|m| m["type"] == "code").cloned());
                            with(m)
                        };
                        let a = marked(&format!("a{ja}"), marks(ea, "/a"));
                        let b = marked(&format!("{jb}b"), marks(eb, "/b"));
                        fixture.push(para(json!([t("p"), a.clone(), b.clone(), t("q")])));
                        expected.push(para(json!([t("p"), a, b, t("q")])));
                        // The same href twice is one link.
                        let a = marked(&format!("a{ja}"), marks(ea, "/a"));
                        let b = marked(&format!("{jb}b"), marks(eb, "/a"));
                        fixture.push(para(json!([a.clone(), b.clone()])));
                        let same = ea == eb;
                        expected.push(match same {
                            true => para(json!([marked(&format!("a{ja}{jb}b"), marks(ea, "/a"))])),
                            false => para(json!([a, b])),
                        });
                    }
                }
            }
        }
        let br = json!({"type": "hardBreak"});
        let bold = json!([{"type": "bold"}]);
        let code = json!([{"type": "code"}]);
        let linked = json!([{"type": "link", "attrs": {"href": "/a"}}]);
        let bold_link = json!([{"type": "bold"}, {"type": "link", "attrs": {"href": "/a"}}]);
        for p in [
            // A mark open across the break.
            json!([marked("a", bold.clone()), br, marked("b", bold.clone())]),
            json!([marked("a ", bold.clone()), br, marked(" b", bold.clone())]),
            // Ending right before it, starting right after it.
            json!([t("x"), marked("a", bold.clone()), br, t("b")]),
            json!([t("a"), br, marked("b", bold.clone()), t("x")]),
            json!([
                marked("a!", bold.clone()),
                br,
                marked("!b", json!([{"type": "italic"}]))
            ]),
            // Code and links around it.
            json!([marked("a", code.clone()), br, marked("b", code.clone())]),
            json!([marked("a ", code.clone()), br, marked(" b", code)]),
            json!([marked("a", linked.clone()), br, marked("b", linked)]),
            json!([marked("a", bold_link.clone()), br, marked("b", bold_link)]),
        ] {
            fixture.push(para(p.clone()));
            expected.push(para(p));
        }
        let markdown = to_markdown(&doc(Value::Array(fixture)), &kind, &ctx);
        assert_eq!(
            from_markdown(&markdown, &kind),
            Ok(doc(Value::Array(expected))),
            "{markdown}"
        );
    }

    /// Text that starts a line the way structure does, after any indent, stays text
    /// wherever the line is (W1-R30).
    #[test]
    fn markdown_line_starts_stay_text() {
        let none = |_: &str, _: &Value| None;
        let ctx = RenderCtx {
            link_url: &none,
            title: &none,
            slug: &none,
        };
        let kind = body().kind;
        let openers = [
            "#", "-", "*", "+", "1.", "1)", ">", "```", "~~~", ":::x", "::x{}", "|", "---", "===",
            "<u>", "<div>",
        ];
        for indent in ["", " ", "  ", "   ", "    ", "\t"] {
            for opener in openers {
                let line = format!("{indent}{opener} b");
                let cell = |kind: &str| json!({"type": kind, "content": [para(json!([t(&line)]))]});
                let fixture = doc(json!([
                    para(json!([t(&line)])),
                    para(json!([t("a"), {"type": "hardBreak"}, t(&line)])),
                    para(json!([t(&format!("a\n{line}"))])),
                    heading(2, &line),
                    {"type": "table", "content": [
                        {"type": "tableRow", "content": [cell("tableHeader")]},
                        {"type": "tableRow", "content": [cell("tableCell")]}
                    ]}
                ]));
                let markdown = to_markdown(&fixture, &kind, &ctx);
                assert_eq!(
                    from_markdown(&markdown, &kind),
                    Ok(fixture),
                    "{line:?}\n{markdown}"
                );
            }
        }
    }

    /// Text holding line breaks that would read as structure if written as they are.
    const BROKEN: &str = "a\n::video{}\r\n# b";

    fn meta(value: Value) -> Value {
        json!({"type": "block", "attrs": {"blockType": "video", "fields": {"url": "u", "meta": value}}})
    }

    #[test]
    fn markdown_input_is_bounded() {
        let b = body();
        let over = "a".repeat(MAX_BYTES + 1);
        assert_eq!(
            from_markdown(&over, &b.kind),
            Err(vec![FieldError {
                path: String::new(),
                message: "Markdown can be 1048576 bytes at most".into()
            }])
        );
        assert!(from_markdown(&"a".repeat(MAX_BYTES), &b.kind).is_ok());
        // The depth limit counts the whole value: nesting inside blocks doesn't reset it.
        let quotes = |n: usize| format!("{}x", "> ".repeat(n));
        let nested = |n: usize| format!("::::callout\n:::aside\n{}\n:::\n::::", quotes(n));
        assert!(from_markdown(&quotes(30), &b.kind).is_ok());
        assert!(from_markdown(&nested(28), &b.kind).is_ok());
        assert_eq!(
            from_markdown(&nested(29), &b.kind).unwrap_err()[0].message,
            "line 3: nests more than 32 levels deep"
        );
        // Input built to be expensive is refused, or read, without blowing up: the
        // limits stop the tree before it grows past them.
        let cap = MAX_BYTES / 4;
        let deep_lines: String = (0..2_000).map(|i| quotes(i % 500) + "\n").collect();
        for (src, refusal) in [
            (
                "> - 1. ".repeat(cap / 7),
                Some("line 1: nests more than 32 levels deep"),
            ),
            (deep_lines, Some("line 32: nests more than 32 levels deep")),
            (
                "::::callout\n:::aside\n:::\n::::\n".repeat(30_000),
                Some("line 99999: holds more than 50000 nodes"),
            ),
            (
                "*a".repeat(cap / 2),
                Some("line 1: holds more than 50000 nodes"),
            ),
            (
                "|a|\n|-|\n".repeat(cap / 8),
                Some("line 12501: holds more than 50000 nodes"),
            ),
            ("[".repeat(cap), None),
            ("[a](".repeat(cap / 4), None),
            ("![".repeat(cap / 2), None),
            ("`".repeat(cap), None),
        ] {
            let shown: String = src.chars().take(20).collect();
            let got = from_markdown(&src, &b.kind)
                .map(|_| ())
                .map_err(|e| e[0].message.clone());
            match refusal {
                Some(m) => assert_eq!(got, Err(m.to_owned()), "{shown}"),
                None => assert!(got.is_ok(), "{shown}: {got:?}"),
            }
        }
    }

    #[test]
    fn markdown_inputs_convert_before_validation() {
        use crate::validate::{markdown_inputs, validate_document};
        let fields: Vec<Field> = serde_json::from_value(json!([
            body(),
            {"name": "layout", "type": "blocks", "blocks": [
                {"slug": "copy", "fields": [{"name": "text", "type": "richText"}]}
            ]}
        ]))
        .unwrap();
        let md = |s: &str| json!({"format": "markdown", "value": s});
        let mut input =
            json!({"body": md("**hi**"), "layout": [{"blockType": "copy", "text": md("*x*")}]});
        assert_eq!(
            markdown_inputs(&fields, &mut input),
            Ok(vec!["body".into(), "layout.0.text".into()])
        );
        assert_eq!(
            input["body"],
            doc(json!([para(json!([marked(
                "hi",
                json!([{"type": "bold"}])
            )]))]))
        );
        let stored = validate_document(&fields, &input, None).unwrap();
        assert_eq!(
            stored["layout"][0]["text"],
            doc(json!([para(json!([marked(
                "x",
                json!([{"type": "italic"}])
            )]))]))
        );

        let refused = |mut input: Value| markdown_inputs(&fields, &mut input).unwrap_err();
        assert_eq!(
            refused(json!({"layout": [{"blockType": "copy", "text": md("ok\n\n<b>no</b>")}]})),
            vec![FieldError {
                path: "layout.0.text".into(),
                message: "line 3: raw HTML isn't allowed; underline is the only tag, as <u>…</u>"
                    .into()
            }]
        );
        assert_eq!(
            refused(json!({"body": md("[x](ftp://a)")})),
            vec![FieldError {
                path: "body.content.0.content.0.marks.0.attrs.href".into(),
                message: "line 1: links must start with http://, https://, mailto:, tel:, / or #"
                    .into()
            }]
        );
        let shape = "expected {\"format\": \"markdown\", \"value\": \"…\"}";
        for bad in [
            json!({"format": "html", "value": "x"}),
            json!({"format": "markdown"}),
            json!({"format": "markdown", "value": "x", "extra": 1}),
        ] {
            assert_eq!(
                refused(json!({ "body": bad })),
                vec![FieldError {
                    path: "body".into(),
                    message: shape.into()
                }]
            );
        }

        // Validation never converts: Markdown that reaches it unconverted skipped the field
        // rules that run on the converted form, so it's refused (W1-R26).
        let unconverted = "Markdown must be converted before validation";
        assert_eq!(
            validate_document(
                &fields,
                &json!({"body": md("hi"), "layout": [{"blockType": "copy", "text": md("x")}]}),
                None
            ),
            Err(vec![
                FieldError {
                    path: "body".into(),
                    message: unconverted.into()
                },
                FieldError {
                    path: "layout.0.text".into(),
                    message: unconverted.into()
                },
            ])
        );
        // Without the API to find it by, a document named by slug is refused.
        let mut input = json!({"body": md("[x](doc:posts/intro)")});
        markdown_inputs(&fields, &mut input).unwrap();
        assert_eq!(
            validate_document(&fields, &input, None).unwrap_err()[0].message,
            "names its document by slug; it must be saved with the document's ID"
        );
    }
}
