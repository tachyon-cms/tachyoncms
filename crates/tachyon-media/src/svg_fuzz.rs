//! Seeded fuzz for the SVG sanitiser: random documents built from the tricks that get past
//! sanitisers (namespaces, mixed case, entities, CDATA, comments, DTDs, `url(` in every
//! spelling). Whatever comes out is parsed again on its own, and must hold nothing that
//! runs or fetches.

use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

use crate::svg::{SvgError, sanitize_svg};

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

const ELEMENTS: &[&str] = &[
    "g",
    "rect",
    "circle",
    "path",
    "text",
    "tspan",
    "use",
    "a",
    "defs",
    "linearGradient",
    "stop",
    "title",
    "script",
    "SCRIPT",
    "Script",
    "svg:script",
    "x:script",
    "foreignObject",
    "foreignobject",
    "FOREIGNOBJECT",
    "svg:foreignObject",
    "html:script",
    "style",
    "set",
    "animate",
    "image",
    "iframe",
    "svg:rect",
    "svg",
];

const ATTRIBUTES: &[&str] = &[
    "fill",
    "stroke",
    "style",
    "transform",
    "width",
    "x",
    "id",
    "href",
    "HREF",
    "xlink:href",
    "XLINK:HREF",
    "Xlink:href",
    "svg:href",
    "x:href",
    "onload",
    "ONLOAD",
    "onClick",
    "svg:onload",
    "xlink:onload",
    "xmlns",
    "xmlns:svg",
    "xmlns:xlink",
    "xmlns:x",
    "xmlns:XLINK",
    "xml:space",
    "attributeName",
    "to",
    "begin",
];

const VALUES: &[&str] = &[
    "#a",
    " #a ",
    "red",
    "javascript:alert(1)",
    " javascript:alert(1)",
    "JaVaScRiPt:alert(1)",
    "java&#x09;script:alert(1)",
    "&#106;avascript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "https://x/y#a",
    "//x#a",
    "url(#a)",
    "url( #a )",
    "url('#a')",
    "url(\"#a\")",
    "url(&quot;#a&quot;)",
    "url(https://x)",
    "URL(//x)",
    "url (//x)",
    "url(\n//x)",
    "url( '//x' )",
    "u&#114;l(//x)",
    "&#x75;rl(//x)",
    r"u\72l(//x)",
    r"\75rl(//x)",
    "ur/**/l(//x)",
    "fill:url(//x)",
    "fill:url(#a);stroke:url(//x)",
    "background:image-set('//x' 1x)",
    "src(//x)",
    "expression(alert(1))",
    "rotate(45)",
    "&xxe;",
    "&lt;script&gt;",
    "&#0;",
    "&#xD800;",
    "http://www.w3.org/2000/svg",
    "http://www.w3.org/1999/xlink",
    "http://www.w3.org/1999/xhtml",
    "onload",
    "",
    "\"",
];

const TEXTS: &[&str] = &[
    "hi",
    " ",
    "\n",
    "&lt;script&gt;",
    "&amp;",
    "&#60;script&#62;",
    "&xxe;",
    "&#0;",
    "<![CDATA[<script>alert(1)</script>]]>",
    "<![CDATA[]]>]]>",
    "<!-- <script>alert(1)</script> -->",
    "<?php echo 1 ?>",
    "<?xml-stylesheet href=\"//x\"?>",
    "a\u{0}b",
];

/// Text that isn't well-formed, picked now and then so most documents still parse.
const BROKEN: &[&str] = &["<!-->", "]]>", "<", "&", "<svg", "</g>", "<a b=c>"];

const PROLOGUES: &[&str] = &[
    "",
    "<?xml version=\"1.0\"?>",
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
    "<!DOCTYPE svg>",
    "<!DOCTYPE svg [<!ENTITY xxe SYSTEM \"file:///etc/passwd\">]>",
    "<!DOCTYPE svg [<!ENTITY x \"<script>alert(1)</script>\">]>",
    "<?xml-stylesheet type=\"text/xsl\" href=\"//x\"?>",
    "<!-- c -->",
    "\u{feff}",
];

fn attrs(rng: &mut Rng, out: &mut String) {
    for _ in 0..rng.n(4) {
        let q = if rng.n(4) == 0 { '\'' } else { '"' };
        let value = rng
            .pick(VALUES)
            .replace(q, if q == '"' { "&quot;" } else { "&apos;" });
        out.push_str(&format!(" {}={q}{value}{q}", rng.pick(ATTRIBUTES)));
    }
}

fn children(rng: &mut Rng, depth: usize, out: &mut String) {
    for _ in 0..rng.n(4) {
        if depth > 0 && rng.n(3) == 0 {
            let name = rng.pick(ELEMENTS);
            out.push('<');
            out.push_str(name);
            attrs(rng, out);
            if rng.n(3) == 0 {
                out.push_str("/>");
            } else {
                out.push('>');
                children(rng, depth - 1, out);
                out.push_str(&format!("</{name}>"));
            }
        } else if rng.n(30) == 0 {
            out.push_str(rng.pick(BROKEN));
        } else {
            out.push_str(rng.pick(TEXTS));
        }
    }
}

fn document(rng: &mut Rng) -> String {
    let mut out = rng.pick(PROLOGUES).to_string();
    let root = rng.pick(&["svg", "svg", "svg", "svg", "svg", "svg:svg", "SVG", "html"]);
    out.push('<');
    out.push_str(root);
    match rng.n(4) {
        0 => {}
        1 => out.push_str(" xmlns=\"http://www.w3.org/2000/svg\""),
        _ => out.push_str(
            " xmlns=\"http://www.w3.org/2000/svg\" xmlns:svg=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:x=\"http://www.w3.org/1999/xhtml\"",
        ),
    }
    attrs(rng, &mut out);
    out.push('>');
    children(rng, 3, &mut out);
    out.push_str(&format!("</{root}>"));
    if rng.n(10) == 0 {
        // Cut off somewhere, char-aligned.
        let mut at = rng.n(out.len() + 1);
        while !out.is_char_boundary(at) {
            at -= 1;
        }
        out.truncate(at);
    }
    out
}

/// Whether CSS-ish `v` points anywhere other than this document.
fn fetches(v: &str) -> bool {
    let squashed: String = v
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if squashed.contains('\\') {
        return true;
    }
    squashed.match_indices("url(").any(|(i, _)| {
        let rest = squashed[i + 4..].trim_start_matches(['"', '\'']);
        !rest.starts_with('#')
    }) || ["image-set(", "src(", "expression(", "image("]
        .iter()
        .any(|f| squashed.contains(f))
}

/// Parses `out` on its own and fails on anything that could run or fetch.
fn check(input: &str, out: &[u8]) {
    let text = std::str::from_utf8(out).unwrap_or_else(|_| panic!("not UTF-8 for {input:?}"));
    let mut r = NsReader::from_str(text);
    let mut depth = 0usize;
    let mut roots = 0;
    loop {
        let (ns, event) = r
            .read_resolved_event()
            .unwrap_or_else(|e| panic!("{e} re-parsing {text:?} from {input:?}"));
        let svg =
            matches!(ns, ResolveResult::Bound(n) if n.into_inner() == "http://www.w3.org/2000/svg");
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                if depth == 0 {
                    roots += 1;
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
                assert!(svg, "an element outside SVG in {text:?} from {input:?}");
                let name = e.local_name().into_inner().to_ascii_lowercase();
                for bad in [
                    "script",
                    "foreignobject",
                    "style",
                    "set",
                    "animate",
                    "iframe",
                    "image",
                ] {
                    assert_ne!(name, bad, "{text:?} from {input:?}");
                }
                for a in e.attributes() {
                    let a = a.unwrap_or_else(|e| panic!("{e} in {text:?} from {input:?}"));
                    let key = a.key.into_inner().to_ascii_lowercase();
                    let local = a.key.local_name().into_inner().to_ascii_lowercase();
                    let value = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .unwrap_or_else(|e| panic!("{e} in {text:?} from {input:?}"));
                    assert!(!local.starts_with("on"), "{key} in {text:?} from {input:?}");
                    if local == "href" {
                        assert!(
                            value.starts_with('#'),
                            "{key}={value:?} in {text:?} from {input:?}"
                        );
                    }
                    if !key.starts_with("xmlns") {
                        assert!(
                            !fetches(&value),
                            "{key}={value:?} in {text:?} from {input:?}"
                        );
                    }
                }
            }
            Event::End(_) => depth -= 1,
            Event::DocType(_) | Event::PI(_) | Event::Decl(_) | Event::Comment(_) => {
                panic!("markup that isn't content in {text:?} from {input:?}")
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert_eq!((roots, depth), (1, 0), "{text:?} from {input:?}");
}

#[test]
fn svg_sanitiser_fuzz() {
    let mut rng = Rng(0x5eed_5a17_0bad_c0de);
    let (mut kept, mut refused) = (0, 0);
    for _ in 0..20_000 {
        let input = document(&mut rng);
        match sanitize_svg(input.as_bytes()) {
            Ok(out) => {
                check(&input, &out);
                assert_eq!(
                    sanitize_svg(&out).as_ref(),
                    Ok(&out),
                    "not idempotent for {input:?}"
                );
                kept += 1;
            }
            Err(SvgError::NotSvg | SvgError::Malformed(_)) => refused += 1,
        }
    }
    // Roots other than `svg`, duplicate attributes and cut-off documents are refused by
    // design; enough of the rest are kept that the checks above see real output.
    assert!(kept > 8_000, "only {kept} kept, {refused} refused");
}
