//! What a file is, from its first bytes alone: never its name or the type a client claims.

/// The type of a file starting with `head` (its first few KiB): images, archives and
/// executables by their magic numbers; markup, scripts and text by how they open; anything
/// else `application/octet-stream`.
pub fn sniff(head: &[u8]) -> &'static str {
    if let Some(t) = infer::get(head) {
        match t.mime_type() {
            // Markup is told apart below: an SVG is XML too.
            "text/xml" | "text/html" | "text/x-shellscript" => {}
            // The spellings the restricted list uses.
            "application/vnd.debian.binary-package" => return "application/x-debian-package",
            "application/x-rpm" => return "application/x-redhat-package-manager",
            "application/x-mach-binary" => return "application/x-executable",
            mime => return mime,
        }
    }
    text(head)
}

/// How the HTML sniffing algorithm recognises a document, each tag followed by a space or `>`.
const HTML_TAGS: &[&str] = &[
    "<!doctype html",
    "<html",
    "<head",
    "<script",
    "<iframe",
    "<h1",
    "<div",
    "<font",
    "<table",
    "<a",
    "<style",
    "<title",
    "<b",
    "<body",
    "<br",
    "<p",
    "<!--",
];

fn text(head: &[u8]) -> &'static str {
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start = head
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(head.len());
    let body = &head[start..];
    let lower = body.to_ascii_lowercase();
    if let Some(line) = lower.strip_prefix(b"#!") {
        let line = line.split(|&b| b == b'\n').next().unwrap_or_default();
        let has = |w: &[u8]| line.windows(w.len()).any(|x| x == w);
        return if has(b"python") {
            "text/x-python"
        } else if has(b"perl") {
            "text/x-perl"
        } else if has(b"ruby") {
            "text/x-ruby"
        } else if has(b"node") {
            "text/javascript"
        } else {
            // Run by whatever the line names: a script either way.
            "text/x-shellscript"
        };
    }
    if lower.starts_with(b"<?php") {
        return "application/x-httpd-php";
    }
    if lower.starts_with(b"<") {
        let find = |w: &[u8]| lower.windows(w.len()).position(|x| x == w);
        let html = HTML_TAGS.iter().any(|t| {
            lower.strip_prefix(t.as_bytes()).is_some_and(|rest| {
                t.ends_with("--")
                    || matches!(
                        rest.first(),
                        Some(b' ' | b'>' | b'/' | b'\t' | b'\n' | b'\r')
                    )
            })
        });
        match (find(b"<svg"), find(b"<html")) {
            (Some(svg), html_at) if !html && html_at.is_none_or(|h| svg < h) => {
                return "image/svg+xml";
            }
            (_, Some(_)) => return "text/html",
            _ if html => return "text/html",
            _ if lower.starts_with(b"<?xml") => return "text/xml",
            _ => {}
        }
    }
    if is_text(head) {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

/// UTF-8, allowing a character cut off at the end of `head`, without control characters
/// other than tab, line breaks and form feed.
fn is_text(head: &[u8]) -> bool {
    let valid = match std::str::from_utf8(head) {
        Ok(s) => s,
        // At most 3 bytes of a character cut off by the head's end.
        Err(e) if e.error_len().is_none() && head.len() - e.valid_up_to() < 4 => {
            std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return false,
    };
    valid
        .chars()
        .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r' | '\x0c'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_come_from_the_bytes() {
        let mut elf = b"\x7fELF\x02\x01\x01".to_vec();
        elf.resize(64, 0);
        let mut macho = b"\xCF\xFA\xED\xFE".to_vec();
        macho.resize(64, 0);
        for (head, want) in [
            (&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..], "image/png"),
            (b"\xFF\xD8\xFF\xE0\0\x10JFIF", "image/jpeg"),
            (b"GIF89a\x01\0\x01\0", "image/gif"),
            (b"%PDF-1.7\n", "application/pdf"),
            (&elf, "application/x-executable"),
            (&macho, "application/x-executable"),
            (
                b"MZ\x90\0\x03\0\0\0",
                "application/vnd.microsoft.portable-executable",
            ),
            (b"<!DOCTYPE html><p>hi", "text/html"),
            (b"  \n<html>", "text/html"),
            (b"<p>hi</p>", "text/html"),
            (b"<script/src=x>", "text/html"),
            (b"<body/onload=x>", "text/html"),
            (b"<!-- x --><div>", "text/html"),
            (
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
                "image/svg+xml",
            ),
            (b"\xEF\xBB\xBF<?xml version=\"1.0\"?><svg>", "image/svg+xml"),
            (b"<?xml version=\"1.0\"?><html><svg>", "text/html"),
            (b"<?xml version=\"1.0\"?><feed/>", "text/xml"),
            (b"#!/bin/sh\necho hi", "text/x-shellscript"),
            (b"#!/usr/bin/env python3\n", "text/x-python"),
            (b"<?php echo 1;", "application/x-httpd-php"),
            (b"hello, world\n", "text/plain"),
            ("caf\u{e9}".as_bytes(), "text/plain"),
            (&"caf\u{e9}".as_bytes()[..4], "text/plain"),
            (b"", "text/plain"),
            (b"\0\x01\x02\x03", "application/octet-stream"),
            (b"<abbr>", "text/plain"),
        ] {
            assert_eq!(sniff(head), want, "{:?}", String::from_utf8_lossy(head));
        }
    }
}
