//! SVG uploads, rewritten to an allowlist: shapes, paint, gradients, text and `use` of a
//! `#fragment`. Anything that could run or fetch (scripts, `foreignObject`, event
//! attributes, links and `url(…)` leaving the document, DTDs, processing instructions)
//! is dropped. An SVG is never rasterised.

use quick_xml::escape::{escape, partial_escape, resolve_predefined_entity};
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};

/// Why an upload isn't a usable SVG.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SvgError {
    /// Its root element isn't `svg`.
    #[error("this file's root element isn't `svg`")]
    NotSvg,
    #[error("this SVG isn't well-formed XML: {0}")]
    Malformed(String),
}

/// A sanitised SVG and the size its root gives in plain numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanitised {
    pub bytes: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// `input` rewritten to the allowlist. Idempotent: sanitising the output changes nothing.
pub fn sanitize_svg(input: &[u8]) -> Result<Vec<u8>, SvgError> {
    sanitize(input).map(|s| s.bytes)
}

/// `sanitize_svg`, with the root's `width` and `height` where they're plain numbers.
pub fn sanitize(input: &[u8]) -> Result<Sanitised, SvgError> {
    let input = input.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(input);
    let text =
        std::str::from_utf8(input).map_err(|_| SvgError::Malformed("it isn't UTF-8".into()))?;
    let mut reader = NsReader::from_str(text);
    let malformed = |e: quick_xml::Error| SvgError::Malformed(e.to_string());
    let mut out = String::with_capacity(text.len());
    // The kept elements open now, and how deep into a dropped one the reader is.
    let mut open: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let (mut rooted, mut width, mut height) = (false, None, None);
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(malformed)?;
        let in_svg = match ns {
            ResolveResult::Unbound => true,
            ResolveResult::Bound(n) => n.into_inner() == SVG_NS,
            ResolveResult::Unknown(_) => false,
        };
        let inside = dropped == 0 && !open.is_empty();
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                if dropped > 0 {
                    dropped += usize::from(!empty);
                    continue;
                }
                let name = e.local_name().into_inner().to_owned();
                if !rooted {
                    if name != "svg" || !in_svg {
                        return Err(SvgError::NotSvg);
                    }
                    rooted = true;
                } else if open.is_empty() {
                    // Past the root's end.
                    break;
                }
                if !in_svg || !ELEMENTS.contains(&name.as_str()) {
                    dropped = usize::from(!empty);
                    continue;
                }
                out.push('<');
                out.push_str(&name);
                if open.is_empty() {
                    out.push_str(&format!(" xmlns=\"{SVG_NS}\" xmlns:xlink=\"{XLINK_NS}\""));
                }
                // Two prefixes bound to one namespace write the same name: the first stays.
                let mut written: Vec<&'static str> = Vec::new();
                for a in e.attributes() {
                    let a = a.map_err(|e| SvgError::Malformed(e.to_string()))?;
                    let Some((key, value)) = kept(&reader, &a) else {
                        continue;
                    };
                    if written.contains(&key) {
                        continue;
                    }
                    written.push(key);
                    if open.is_empty() {
                        match key {
                            "width" => width = plain_number(&value),
                            "height" => height = plain_number(&value),
                            _ => {}
                        }
                    }
                    // Written as references, these come back as themselves rather than as
                    // the spaces a parser makes of them.
                    let value = escape(&value)
                        .replace('\n', "&#10;")
                        .replace('\t', "&#9;")
                        .replace('\r', "&#13;");
                    out.push_str(&format!(" {key}=\"{value}\""));
                }
                if empty {
                    out.push_str("/>");
                    if open.is_empty() {
                        break;
                    }
                } else {
                    out.push('>');
                    open.push(name);
                }
            }
            Event::End(_) => {
                if dropped > 0 {
                    dropped -= 1;
                    continue;
                }
                let Some(name) = open.pop() else {
                    return Err(SvgError::Malformed("an end tag closes nothing".into()));
                };
                out.push_str(&format!("</{name}>"));
                if open.is_empty() {
                    break;
                }
            }
            Event::Text(t) if inside => {
                out.push_str(&partial_escape(xml_chars(&t.xml10_content())))
            }
            Event::CData(t) if inside => {
                out.push_str(&partial_escape(xml_chars(&t.xml10_content())))
            }
            Event::GeneralRef(r) if inside => {
                let c = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    Ok(None) => {
                        let name = r.xml10_content();
                        resolve_predefined_entity(&name).map(str::to_owned)
                    }
                    Err(_) => None,
                };
                // An entity a DTD would define is dropped with the DTD.
                if let Some(c) = c {
                    out.push_str(&partial_escape(xml_chars(&c)));
                }
            }
            Event::Eof => break,
            // Comments, the declaration, DTDs and processing instructions go, as does
            // whatever is outside the root or inside a dropped element.
            _ => {}
        }
    }
    if !rooted {
        return Err(SvgError::NotSvg);
    }
    if !open.is_empty() || dropped > 0 {
        return Err(SvgError::Malformed("it ends before its root does".into()));
    }
    Ok(Sanitised {
        bytes: out.into_bytes(),
        width,
        height,
    })
}

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// SVG 1.1's shapes, structure, paint servers, clipping, masking, filters and text. Not
/// `script`, `style`, `foreignObject`, `image`, animation (`set` can rewrite an `href`),
/// or anything else that runs or fetches.
const ELEMENTS: &[&str] = &[
    "svg",
    "g",
    "defs",
    "title",
    "desc",
    "symbol",
    "use",
    "a",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "text",
    "tspan",
    "textPath",
    "linearGradient",
    "radialGradient",
    "stop",
    "pattern",
    "clipPath",
    "mask",
    "marker",
    "filter",
    "feBlend",
    "feColorMatrix",
    "feComponentTransfer",
    "feFuncR",
    "feFuncG",
    "feFuncB",
    "feFuncA",
    "feComposite",
    "feConvolveMatrix",
    "feDiffuseLighting",
    "feDisplacementMap",
    "feDistantLight",
    "feFlood",
    "feGaussianBlur",
    "feMerge",
    "feMergeNode",
    "feMorphology",
    "feOffset",
    "fePointLight",
    "feSpecularLighting",
    "feSpotLight",
    "feTile",
    "feTurbulence",
];

/// Geometry, presentation and text attributes, in no namespace. `href` is handled apart.
const ATTRIBUTES: &[&str] = &[
    "id",
    "class",
    "style",
    "transform",
    "viewBox",
    "preserveAspectRatio",
    "version",
    "width",
    "height",
    "x",
    "y",
    "x1",
    "y1",
    "x2",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "fx",
    "fy",
    "fr",
    "d",
    "points",
    "pathLength",
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-opacity",
    "opacity",
    "color",
    "display",
    "visibility",
    "overflow",
    "clip",
    "clip-path",
    "clip-rule",
    "mask",
    "filter",
    "marker-start",
    "marker-mid",
    "marker-end",
    "stop-color",
    "stop-opacity",
    "flood-color",
    "flood-opacity",
    "lighting-color",
    "color-interpolation",
    "color-interpolation-filters",
    "color-rendering",
    "shape-rendering",
    "text-rendering",
    "image-rendering",
    "vector-effect",
    "paint-order",
    "gradientUnits",
    "gradientTransform",
    "spreadMethod",
    "offset",
    "patternUnits",
    "patternContentUnits",
    "patternTransform",
    "clipPathUnits",
    "maskUnits",
    "maskContentUnits",
    "markerUnits",
    "markerWidth",
    "markerHeight",
    "refX",
    "refY",
    "orient",
    "filterUnits",
    "primitiveUnits",
    "in",
    "in2",
    "result",
    "stdDeviation",
    "dx",
    "dy",
    "mode",
    "type",
    "values",
    "operator",
    "k1",
    "k2",
    "k3",
    "k4",
    "radius",
    "baseFrequency",
    "numOctaves",
    "seed",
    "stitchTiles",
    "scale",
    "xChannelSelector",
    "yChannelSelector",
    "tableValues",
    "slope",
    "intercept",
    "amplitude",
    "exponent",
    "order",
    "kernelMatrix",
    "divisor",
    "bias",
    "targetX",
    "targetY",
    "edgeMode",
    "kernelUnitLength",
    "preserveAlpha",
    "surfaceScale",
    "diffuseConstant",
    "specularConstant",
    "specularExponent",
    "azimuth",
    "elevation",
    "z",
    "pointsAtX",
    "pointsAtY",
    "pointsAtZ",
    "limitingConeAngle",
    "font-family",
    "font-size",
    "font-size-adjust",
    "font-style",
    "font-weight",
    "font-variant",
    "font-stretch",
    "text-anchor",
    "dominant-baseline",
    "alignment-baseline",
    "baseline-shift",
    "letter-spacing",
    "word-spacing",
    "text-decoration",
    "writing-mode",
    "direction",
    "unicode-bidi",
    "rotate",
    "textLength",
    "lengthAdjust",
    "startOffset",
    "method",
    "spacing",
    "systemLanguage",
];

/// The name `a` is written under and its value, when it's on the allowlist and its value
/// can't run or fetch anything.
fn kept(reader: &NsReader<&[u8]>, a: &Attribute) -> Option<(&'static str, String)> {
    if a.key.as_namespace_binding().is_some() {
        return None;
    }
    let (ns, local) = reader.resolver().resolve_attribute(a.key);
    let local = local.into_inner();
    let key: &'static str = match ns {
        ResolveResult::Unbound if local == "href" => "href",
        ResolveResult::Unbound => ATTRIBUTES.iter().find(|n| **n == local)?,
        ResolveResult::Bound(n) if n.into_inner() == XLINK_NS && local == "href" => "xlink:href",
        ResolveResult::Bound(n) if n.into_inner() == XML_NS && local == "space" => "xml:space",
        ResolveResult::Bound(n) if n.into_inner() == XML_NS && local == "lang" => "xml:lang",
        _ => return None,
    };
    // An entity a DTD would define, or a character XML can't hold: the attribute goes.
    let value = a.normalized_value(XmlVersion::Implicit1_0).ok()?;
    if value.chars().any(|c| !xml_char(c)) {
        return None;
    }
    if key.ends_with("href") {
        let target = value.trim_matches(|c: char| c.is_ascii_whitespace());
        return target.starts_with('#').then(|| (key, target.to_owned()));
    }
    stays_local(&value).then(|| (key, value.into_owned()))
}

/// Whether CSS-ish `v` only calls functions that stay in the document: `url(#…)`, colours
/// and transforms. CSS escapes and comments, which can spell any of the others, count as
/// leaving.
fn stays_local(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    if lower.contains('\\') || lower.contains("/*") || lower.contains('@') {
        return false;
    }
    lower.match_indices('(').all(|(i, _)| {
        let before = lower[..i].trim_end();
        // Just past the last character that can't be in a name, which may be wider than
        // one byte.
        let start = before
            .char_indices()
            .rev()
            .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .map_or(0, |(s, c)| s + c.len_utf8());
        let name = &before[start..];
        if name == "url" {
            let inner = lower[i + 1..]
                .trim_start()
                .trim_start_matches(['"', '\''])
                .trim_start();
            return inner.starts_with('#');
        }
        FUNCTIONS.contains(&name)
    })
}

/// Functions a paint, length or transform may call.
const FUNCTIONS: &[&str] = &[
    "rgb",
    "rgba",
    "hsl",
    "hsla",
    "matrix",
    "translate",
    "translatex",
    "translatey",
    "scale",
    "scalex",
    "scaley",
    "rotate",
    "skewx",
    "skewy",
];

/// A positive whole-ish number, like `120` or `80.5`: no units, no percent.
fn plain_number(v: &str) -> Option<u32> {
    let v = v.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let ok = match v.split_once('.') {
        Some((whole, frac)) => digits(whole) && digits(frac),
        None => digits(v),
    };
    let n: f64 = v.parse().ok().filter(|_| ok)?;
    let n = n.round();
    (n >= 1.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
}

/// Characters XML 1.0 can hold.
fn xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

fn xml_chars(s: &str) -> String {
    s.chars().filter(|&c| xml_char(c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        String::from_utf8(sanitize_svg(s.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn svg_is_sanitised() {
        let dirty = r#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"><script>alert(2)</script><foreignObject><div>x</div></foreignObject><a href="javascript:alert(3)"/><use href="https://x/y#a"/><rect width="1" fill="url(#g)"/></svg>"#;
        let out = clean(dirty);
        for gone in [
            "onload",
            "script",
            "alert",
            "foreignObject",
            "javascript",
            "https://x",
        ] {
            assert!(!out.contains(gone), "{gone} survived: {out}");
        }
        assert!(
            out.contains(r##"<rect width="1" fill="url(#g)"/>"##),
            "{out}"
        );
        assert!(out.contains("<use/>"), "{out}");
        assert_eq!(clean(&out), out);
    }

    #[test]
    fn fragment_links_stay() {
        let out = clean(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:l="http://www.w3.org/1999/xlink"><use l:href="#a"/><use href=" #b"/></svg>"##,
        );
        assert!(out.contains(r##"<use xlink:href="#a"/>"##), "{out}");
        assert!(out.contains(r##"<use href="#b"/>"##), "{out}");
    }

    #[test]
    fn smuggled_urls_are_dropped() {
        for value in [
            "url(https://x)",
            "URL( 'https://x' )",
            "url (//x)",
            "u&#114;l(//x)",
            r"u\72l(//x)",
            "image-set(&quot;//x&quot; 1x)",
            "src(//x)",
            "url(#a) url(//x)",
        ] {
            let out = clean(&format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="{value}" style="fill:{value}"/></svg>"#
            ));
            assert!(out.contains("<rect/>"), "{value}: {out}");
        }
        let out = clean(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><rect style="fill: url( &quot;#a&quot; )" transform="rotate(45) translate(1, 2)"/></svg>"##,
        );
        assert!(out.contains("style="), "{out}");
        assert!(out.contains("transform="), "{out}");
    }

    #[test]
    fn non_ascii_before_a_bracket_doesnt_panic() {
        for value in [
            "éurl(//x)",
            "font-family:é(x)",
            "ＵＲＬ(//x)",
            "a\u{301}url(//x)",
        ] {
            let out = clean(&format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="{value}"/></svg>"#
            ));
            assert!(out.contains("<rect/>"), "{value}: {out}");
        }
    }

    #[test]
    fn aliased_prefixes_dont_repeat_an_attribute() {
        let out = clean(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:a="http://www.w3.org/1999/xlink" xmlns:b="http://www.w3.org/1999/xlink"><use a:href="#x" b:href="#y"/></svg>"##,
        );
        assert!(out.contains(r##"<use xlink:href="#x"/>"##), "{out}");
        assert_eq!(clean(&out), out);
    }

    #[test]
    fn whitespace_references_survive_a_second_pass() {
        let out = clean(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect id="a&#10;b&#9;c&#13;d"/></svg>"#,
        );
        assert!(out.contains(r#"id="a&#10;b&#9;c&#13;d""#), "{out}");
        assert_eq!(clean(&out), out);
    }

    #[test]
    fn namespaced_and_cased_scripts_go() {
        let out = clean(
            r#"<svg:svg xmlns:svg="http://www.w3.org/2000/svg"><svg:script>a()</svg:script><SCRIPT>b()</SCRIPT><svg:rect ONLOAD="c()" onClick="d()" XLINK:HREF="//x"/></svg:svg>"#,
        );
        assert_eq!(
            out,
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><rect/></svg>"#
        );
    }

    #[test]
    fn dtds_entities_cdata_and_pis_go() {
        let out = clean(
            "<?xml version=\"1.0\"?><!DOCTYPE svg [<!ENTITY x \"<script>e()</script>\">]><?xml-stylesheet href=\"//x\"?>\
             <svg xmlns=\"http://www.w3.org/2000/svg\"><!-- <script> --><text>&x;&lt;b&#62;<![CDATA[<script>f()</script>]]></text></svg>",
        );
        assert!(!out.contains("<script"), "{out}");
        assert!(!out.contains("DOCTYPE") && !out.contains("<?") && !out.contains("<!--"));
        assert!(
            out.contains("<text>&lt;b&gt;&lt;script&gt;f()&lt;/script&gt;</text>"),
            "{out}"
        );
        assert_eq!(clean(&out), out);
    }

    #[test]
    fn the_root_has_to_be_svg() {
        assert_eq!(sanitize_svg(b"<html><svg/></html>"), Err(SvgError::NotSvg));
        assert_eq!(sanitize_svg(b"just text"), Err(SvgError::NotSvg));
        assert!(matches!(
            sanitize_svg(b"<svg><g></svg>"),
            Err(SvgError::Malformed(_))
        ));
        assert!(matches!(
            sanitize_svg(b"<svg><g>"),
            Err(SvgError::Malformed(_))
        ));
    }

    #[test]
    fn size_comes_from_plain_numbers() {
        let size = |attrs: &str| {
            let s = sanitize(format!("<svg {attrs}/>").as_bytes()).unwrap();
            (s.width, s.height)
        };
        assert_eq!(size(r#"width="120" height="80.4""#), (Some(120), Some(80)));
        assert_eq!(size(r#"width="10em" height="50%""#), (None, None));
        assert_eq!(size(r#"viewBox="0 0 10 10""#), (None, None));
        assert_eq!(size(r#"width="0" height="-3""#), (None, None));
    }
}
