//! Upload collections: the `upload` option, the fields it adds, and the rules for the names
//! and types of the files they hold. This module does no I/O.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::schema::{Admin, Field, FieldKind};

/// `upload` on a collection: where its files go and what's made from them. Names and
/// meanings follow the reference's, so a config written for it reads the same here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Upload {
    /// The `[storage.<name>]` the files go to.
    pub storage: String,
    /// Allowed types, such as `image/*` or `application/pdf`; empty allows any.
    pub mime_types: Vec<String>,
    /// Bytes; the instance's `[uploads] max_file_size` when unset.
    pub filesize_limit: Option<u64>,
    pub image_sizes: Vec<ImageSize>,
    /// The size the admin shows as the thumbnail.
    pub admin_thumbnail: Option<String>,
    pub crop: bool,
    pub focal_point: bool,
    pub format_options: Option<FormatOptions>,
    pub resize_options: Option<ResizeOptions>,
    pub trim_options: Option<TrimOptions>,
    pub with_metadata: bool,
    pub constructor_options: Option<ConstructorOptions>,
    pub static_dir: Option<String>,
    pub disable_local_storage: bool,
    pub files_required_on_create: bool,
    pub filename_compound_index: Option<Vec<String>>,
    #[serde(rename = "pasteURL")]
    pub paste_url: PasteUrl,
    pub skip_safe_fetch: SkipSafeFetch,
    pub allow_restricted_file_types: bool,
    pub bulk_upload: bool,
    pub display_preview: bool,
    pub hide_file_input_on_create: bool,
    pub hide_remove_file: bool,
    pub cache_tags: bool,
    /// Headers merged into every file response.
    pub modify_response_headers: BTreeMap<String, String>,
}

impl Default for Upload {
    fn default() -> Self {
        Self {
            storage: "default".into(),
            mime_types: Vec::new(),
            filesize_limit: None,
            image_sizes: Vec::new(),
            admin_thumbnail: None,
            crop: true,
            focal_point: true,
            format_options: None,
            resize_options: None,
            trim_options: None,
            with_metadata: false,
            constructor_options: None,
            static_dir: None,
            disable_local_storage: false,
            files_required_on_create: true,
            filename_compound_index: None,
            paste_url: PasteUrl::On,
            skip_safe_fetch: SkipSafeFetch::No,
            allow_restricted_file_types: false,
            bulk_upload: false,
            display_preview: false,
            hide_file_input_on_create: false,
            hide_remove_file: false,
            cache_tags: false,
            modify_response_headers: BTreeMap::new(),
        }
    }
}

/// One image made from each upload, such as a `thumbnail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSize {
    pub name: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub fit: Fit,
    #[serde(default)]
    pub position: Position,
    #[serde(default)]
    pub without_enlargement: Option<bool>,
    #[serde(default)]
    pub format_options: Option<FormatOptions>,
    #[serde(default)]
    pub crop: Option<Position>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    #[default]
    Cover,
    Contain,
    Fill,
    Inside,
    Outside,
}

/// Where a crop keeps the image, by sharp's names; its compass names are taken too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Position {
    #[default]
    #[serde(rename = "centre", alias = "center")]
    Centre,
    #[serde(rename = "top", alias = "north")]
    Top,
    #[serde(rename = "right top", alias = "northeast")]
    RightTop,
    #[serde(rename = "right", alias = "east")]
    Right,
    #[serde(rename = "right bottom", alias = "southeast")]
    RightBottom,
    #[serde(rename = "bottom", alias = "south")]
    Bottom,
    #[serde(rename = "left bottom", alias = "southwest")]
    LeftBottom,
    #[serde(rename = "left", alias = "west")]
    Left,
    #[serde(rename = "left top", alias = "northwest")]
    LeftTop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatOptions {
    pub format: ImageFormat,
    #[serde(default)]
    pub options: EncodeOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeOptions {
    pub quality: Option<u8>,
    pub lossless: bool,
    pub effort: Option<u8>,
    pub progressive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[serde(alias = "jpg")]
    Jpeg,
    Png,
    Webp,
    Avif,
    Gif,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResizeOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fit: Fit,
    pub position: Position,
    pub without_enlargement: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrimOptions {
    pub background: Option<String>,
    pub threshold: Option<u8>,
}

/// The decoder options that mean something without sharp.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConstructorOptions {
    pub limit_input_pixels: Option<u64>,
    pub fail_on: FailOn,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailOn {
    None,
    Truncated,
    Error,
    #[default]
    Warning,
}

/// `pasteURL`: `false`, `true` or `{"allowList": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PasteUrlShape", into = "PasteUrlShape")]
pub enum PasteUrl {
    Off,
    #[default]
    On,
    AllowList(Vec<UrlPattern>),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PasteUrlShape {
    Flag(bool),
    List {
        #[serde(rename = "allowList")]
        allow_list: Vec<UrlPattern>,
    },
}

impl From<PasteUrlShape> for PasteUrl {
    fn from(s: PasteUrlShape) -> Self {
        match s {
            PasteUrlShape::Flag(false) => Self::Off,
            PasteUrlShape::Flag(true) => Self::On,
            PasteUrlShape::List { allow_list } => Self::AllowList(allow_list),
        }
    }
}

impl From<PasteUrl> for PasteUrlShape {
    fn from(p: PasteUrl) -> Self {
        match p {
            PasteUrl::Off => Self::Flag(false),
            PasteUrl::On => Self::Flag(true),
            PasteUrl::AllowList(allow_list) => Self::List { allow_list },
        }
    }
}

/// `skipSafeFetch`: `false`, `true`, or a list of the URLs the fetch guard skips.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "SkipShape", into = "SkipShape")]
pub enum SkipSafeFetch {
    #[default]
    No,
    All,
    AllowList(Vec<UrlPattern>),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SkipShape {
    Flag(bool),
    List(Vec<UrlPattern>),
}

impl From<SkipShape> for SkipSafeFetch {
    fn from(s: SkipShape) -> Self {
        match s {
            SkipShape::Flag(false) => Self::No,
            SkipShape::Flag(true) => Self::All,
            SkipShape::List(list) => Self::AllowList(list),
        }
    }
}

impl From<SkipSafeFetch> for SkipShape {
    fn from(s: SkipSafeFetch) -> Self {
        match s {
            SkipSafeFetch::No => Self::Flag(false),
            SkipSafeFetch::All => Self::Flag(true),
            SkipSafeFetch::AllowList(list) => Self::List(list),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlPattern {
    #[serde(default)]
    pub protocol: Option<String>,
    pub hostname: String,
    #[serde(default)]
    pub port: Option<String>,
    #[serde(default)]
    pub pathname: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
}

/// The fields every upload collection has.
pub const UPLOAD_FIELDS: &[&str] = &[
    "filename",
    "mimeType",
    "filesize",
    "width",
    "height",
    "focalX",
    "focalY",
    "url",
    "thumbnailURL",
    "sizes",
    "blurhash",
];

/// Built-in fields only the server sets.
pub const SERVER_SET: &[&str] = &[
    "filename", "mimeType", "filesize", "width", "height", "sizes", "blurhash",
];

/// Executables and scripts refused unless `allowRestrictedFileTypes`, as (extension, type).
/// The reference's list, plus shell scripts.
pub const RESTRICTED: &[(&str, &str)] = &[
    ("exe", "application/x-msdownload"),
    ("dll", "application/x-msdownload"),
    ("exe", "application/x-executable"),
    ("com", "application/x-executable"),
    ("app", "application/x-executable"),
    ("action", "application/x-executable"),
    ("bat", "application/x-msdos-program"),
    ("cmd", "application/x-msdos-program"),
    ("exe", "application/x-ms-dos-executable"),
    ("com", "application/x-ms-dos-executable"),
    ("dmg", "application/x-apple-diskimage"),
    ("deb", "application/x-debian-package"),
    ("rpm", "application/x-redhat-package-manager"),
    ("exe", "application/vnd.microsoft.portable-executable"),
    ("dll", "application/vnd.microsoft.portable-executable"),
    ("msi", "application/x-msi"),
    ("jar", "application/java-archive"),
    ("ear", "application/java-archive"),
    ("war", "application/java-archive"),
    ("desktop", "application/x-desktop"),
    ("cpl", "application/x-cpl"),
    ("lnk", "application/x-ms-shortcut"),
    ("pkg", "application/x-apple-installer"),
    ("htm", "text/html"),
    ("html", "text/html"),
    ("shtml", "text/html"),
    ("xhtml", "text/html"),
    ("php", "application/x-httpd-php"),
    ("phtml", "application/x-httpd-php"),
    ("js", "text/javascript"),
    ("jse", "text/javascript"),
    ("jsp", "application/x-jsp"),
    ("py", "text/x-python"),
    ("rb", "text/x-ruby"),
    ("pl", "text/x-perl"),
    ("ps1", "application/x-powershell"),
    ("psc1", "application/x-powershell"),
    ("psd1", "application/x-powershell"),
    ("psh", "application/x-powershell"),
    ("psm1", "application/x-powershell"),
    ("vbe", "application/x-vbscript"),
    ("vbs", "application/x-vbscript"),
    ("ws", "application/x-ms-wsh"),
    ("wsc", "application/x-ms-wsh"),
    ("wsf", "application/x-ms-wsh"),
    ("wsh", "application/x-ms-wsh"),
    ("scr", "application/x-msdownload"),
    ("asp", "application/x-asp"),
    ("aspx", "application/x-asp"),
    ("hta", "application/x-hta"),
    ("reg", "application/x-registry"),
    ("url", "application/x-url"),
    ("workflow", "application/x-workflow"),
    ("command", "application/x-command"),
    // Ours: shell scripts, which the reference's list leaves out.
    ("sh", "text/x-shellscript"),
    ("sh", "application/x-sh"),
    ("bash", "application/x-sh"),
    ("zsh", "application/x-sh"),
];

fn built_in(name: &str, kind: FieldKind) -> Field {
    Field {
        name: name.into(),
        label: None,
        required: false,
        default: None,
        unique: false,
        filterable: false,
        admin: Admin {
            read_only: SERVER_SET.contains(&name),
            ..Admin::default()
        },
        virtual_path: None,
        computed: false,
        access: None,
        kind,
    }
}

fn text(name: &str) -> Field {
    built_in(
        name,
        FieldKind::Text {
            min_length: None,
            max_length: None,
            has_many: false,
            min_rows: None,
            max_rows: None,
        },
    )
}

fn number(name: &str, min: Option<f64>, max: Option<f64>, integer: bool) -> Field {
    built_in(
        name,
        FieldKind::Number {
            min,
            max,
            integer,
            has_many: false,
            min_rows: None,
            max_rows: None,
        },
    )
}

/// A value filled on read, such as `url`: never stored.
fn computed(name: &str) -> Field {
    let mut f = text(name);
    f.computed = true;
    f.admin.read_only = true;
    f
}

fn group(name: &str, fields: Vec<Field>) -> Field {
    built_in(name, FieldKind::Group { fields })
}

/// What's known about one stored image: a size, or one of its formats.
fn image_entry() -> Vec<Field> {
    vec![
        computed("url"),
        number("width", Some(0.0), None, true),
        number("height", Some(0.0), None, true),
        text("mimeType"),
        number("filesize", Some(0.0), None, true),
        text("filename"),
    ]
}

fn built_ins(u: &Upload) -> Vec<Field> {
    let sizes = u
        .image_sizes
        .iter()
        .map(|s| {
            let mut entry = image_entry();
            entry.push(group(
                "formats",
                vec![group("avif", image_entry()), group("webp", image_entry())],
            ));
            group(&s.name, entry)
        })
        .collect();
    let focal = |name| {
        let mut f = number(name, Some(0.0), Some(100.0), false);
        f.default = Some(json!(50));
        f
    };
    vec![
        text("filename"),
        text("mimeType"),
        number("filesize", Some(0.0), None, true),
        number("width", Some(0.0), None, true),
        number("height", Some(0.0), None, true),
        focal("focalX"),
        focal("focalY"),
        computed("url"),
        computed("thumbnailURL"),
        group("sizes", sizes),
        text("blurhash"),
    ]
}

/// `user` (data fields) with the built-in upload fields: a user field of the same name keeps
/// its `label`, `admin` and `access` but takes the built-in's type; the rest are appended.
/// Whether a user field tries another type is `check`'s to refuse.
pub fn upload_fields(u: &Upload, user: &[Field]) -> Vec<Field> {
    let mut out = user.to_vec();
    for b in built_ins(u) {
        match out.iter_mut().find(|f| f.name == b.name) {
            Some(f) => {
                let read_only = b.admin.read_only;
                *f = Field {
                    label: f.label.take(),
                    admin: std::mem::take(&mut f.admin),
                    access: f.access.take(),
                    ..b
                };
                f.admin.read_only |= read_only;
            }
            None => out.push(b),
        }
    }
    out
}

/// The type the built-in field `name` has, when it's one.
pub(crate) fn built_in_kind(u: &Upload, name: &str) -> Option<&'static str> {
    built_ins(u)
        .into_iter()
        .find(|f| f.name == name)
        .map(|f| f.kind.name())
}

const MAX_FILENAME: usize = 255;

/// `name` split at its last dot into stem and extension; no extension without a stem.
fn split(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The longest extension kept whole; a longer one is cut so the stem survives.
const MAX_EXTENSION: usize = 32;

/// `stem` + `suffix` + `.ext`, the stem cut so the whole fits `MAX_FILENAME` bytes. Never
/// empty, and never starts with a dot or ends with a dot or space, however it's cut.
fn compose(stem: &str, suffix: &str, ext: &str) -> String {
    let ext = cut(ext, MAX_EXTENSION);
    let tail = if ext.is_empty() {
        suffix.to_owned()
    } else {
        format!("{suffix}.{ext}")
    };
    let stem = cut(stem, MAX_FILENAME.saturating_sub(tail.len()));
    let name = format!("{stem}{tail}");
    let name = cut(&name, MAX_FILENAME)
        .trim_start_matches('.')
        .trim_end_matches(['.', ' ']);
    if name.is_empty() {
        "file".into()
    } else {
        name.to_owned()
    }
}

/// Characters that reorder or hide text: they make `a\u{202E}gnp.exe` show as `aexe.png`.
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

const WINDOWS_RESERVED: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CONIN$",
    "CONOUT$",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// An uploaded name made safe to show and to put in a URL or storage key: its last path
/// part, without control or bidi characters, characters Windows refuses, leading dots or a
/// reserved Windows name, at most 255 bytes with its extension kept. Never empty.
pub fn sanitize_filename(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last
        .chars()
        .filter(|c| !invisible(*c) && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .collect();
    let clean = clean
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' ']);
    if clean.is_empty() {
        return "file".into();
    }
    let (stem, ext) = split(clean);
    // Windows reserves `CON` before any extension, `CON.tar.gz` too, so the mark goes there.
    let base = stem.split('.').next().unwrap_or(stem);
    if WINDOWS_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(base.trim_end()))
    {
        let marked = format!("{}-file{}", base.trim_end(), &stem[base.len()..]);
        return compose(&marked, "", ext);
    }
    compose(stem, "", ext)
}

/// The `n`th name tried when `filename` is taken: `a.png` → `a-2.png`.
pub fn numbered(filename: &str, n: u32) -> String {
    let (stem, ext) = split(filename);
    compose(stem, &format!("-{n}"), ext)
}

/// The name of a size made from `filename`: `a.png` at 640×400 as WebP → `a-640x400.webp`.
pub fn size_filename(filename: &str, width: u32, height: u32, ext: &str) -> String {
    let (stem, _) = split(filename);
    compose(stem, &format!("-{width}x{height}"), ext)
}

/// `mime` without parameters, lowercased.
fn essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether `mime` matches one of `patterns`, such as `image/*`; no patterns allow any type.
pub fn mime_allowed(patterns: &[String], mime: &str) -> bool {
    let mime = essence(mime);
    patterns.is_empty()
        || patterns.iter().any(|p| {
            let p = essence(p);
            match p.strip_suffix("/*") {
                Some(top) => top == "*" || mime.split('/').next() == Some(top),
                None => p == "*" || p == mime,
            }
        })
}

/// Whether the sniffed `mime`, or `filename`'s extension, is on the restricted list.
pub fn restricted(mime: &str, filename: &str) -> bool {
    let mime = essence(mime);
    let (_, ext) = split(filename);
    RESTRICTED
        .iter()
        .any(|(e, m)| *m == mime || e.eq_ignore_ascii_case(ext))
}

/// Header names `modifyResponseHeaders` can't set: the server's own safety headers.
pub const PROTECTED_HEADERS: &[&str] = &[
    "content-type",
    "content-security-policy",
    "x-content-type-options",
    "content-disposition",
    "cache-control",
    "set-cookie",
];

/// Checks `u` on its own; `fields` are the collection's data fields with the built-ins in.
/// An error message starts with the option's path, such as `upload.imageSizes.0.name`.
pub(crate) fn check(u: &Upload, fields: &[Field]) -> Result<(), String> {
    let mut names = Vec::new();
    for (i, s) in u.image_sizes.iter().enumerate() {
        let at = format!("upload.imageSizes.{i}");
        if !crate::schema::is_identifier(&s.name, true) {
            return Err(format!(
                "{at}.name: `{}` must start with a letter and use only letters, digits and underscores",
                s.name
            ));
        }
        if names.contains(&s.name.as_str()) {
            return Err(format!("{at}.name: `{}` is used by another size", s.name));
        }
        names.push(&s.name);
        if s.width.is_none() && s.height.is_none() {
            return Err(format!("{at}: needs a width, a height or both"));
        }
    }
    if let Some(t) = &u.admin_thumbnail
        && !names.contains(&t.as_str())
    {
        return Err(format!(
            "upload.adminThumbnail: `{t}` isn't one of the imageSizes"
        ));
    }
    for (i, name) in u.filename_compound_index.iter().flatten().enumerate() {
        let stored = fields.iter().any(|f| {
            &f.name == name
                && !f.computed
                && f.virtual_path.is_none()
                && !matches!(f.kind, FieldKind::Join { .. })
        });
        if !stored {
            return Err(format!(
                "upload.filenameCompoundIndex.{i}: `{name}` must name a stored top-level field"
            ));
        }
    }
    for (name, value) in &u.modify_response_headers {
        let at = format!("upload.modifyResponseHeaders.{name}");
        if PROTECTED_HEADERS
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name))
        {
            return Err(format!("{at}: the server sets this header itself"));
        }
        let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
        if name.is_empty() || !name.chars().all(token) {
            return Err(format!("{at}: isn't a valid header name"));
        }
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(format!(
                "{at}: a header value can't hold control characters"
            ));
        }
    }
    Ok(())
}

/// Checks one raw option value, named by its path, such as `upload.imageSizes.0.fit`.
type Check = fn(&Value, &str) -> Result<(), String>;

/// `v` parses as `T`, or an error at `path` in words rather than serde's type names.
fn leaf<T: serde::de::DeserializeOwned>(v: &Value, path: &str) -> Result<(), String> {
    T::deserialize(v).map(|_| ()).map_err(|e| {
        let mut m = e.to_string();
        for (from, to) in [
            ("unknown variant", "unknown value"),
            ("expected u8", "expected a whole number from 0 to 255"),
            ("expected u32", "expected a whole number"),
            ("expected u64", "expected a whole number"),
            ("a sequence", "a list"),
            ("a map", "an object"),
        ] {
            m = m.replace(from, to);
        }
        format!("{path}: {m}")
    })
}

/// `v` is an object (or `null`, when `nullable`) whose `required` keys are present and whose
/// known keys pass their checks. Unknown keys are `reject_unknown_options`'s.
fn object(
    v: &Value,
    path: &str,
    nullable: bool,
    required: &[&str],
    keys: &[(&str, Check)],
) -> Result<(), String> {
    let o = match v {
        Value::Null if nullable => return Ok(()),
        Value::Object(o) => o,
        _ => return Err(format!("{path}: expected an object")),
    };
    if let Some(r) = required.iter().find(|r| !o.contains_key(**r)) {
        return Err(format!("{path}.{r}: is required"));
    }
    for (k, v) in o {
        if let Some((_, check)) = keys.iter().find(|(name, _)| name == k) {
            check(v, &format!("{path}.{k}"))?;
        }
    }
    Ok(())
}

/// `v` is a list, each item passing `item`.
fn list(v: &Value, path: &str, item: Check) -> Result<(), String> {
    let Value::Array(items) = v else {
        return leaf::<Vec<Value>>(v, path);
    };
    for (i, v) in items.iter().enumerate() {
        item(v, &format!("{path}.{i}"))?;
    }
    Ok(())
}

fn position(v: &Value, path: &str) -> Result<(), String> {
    if let Value::String(p) = v
        && matches!(p.as_str(), "attention" | "entropy")
    {
        return Err(format!(
            "{path}: `{p}` is a strategy only sharp has; name a position such as centre or top"
        ));
    }
    leaf::<Option<Position>>(v, path)
}

fn url_pattern(v: &Value, path: &str) -> Result<(), String> {
    let text: Check = leaf::<Option<String>>;
    object(
        v,
        path,
        false,
        &["hostname"],
        &[
            ("protocol", text),
            ("hostname", leaf::<String>),
            ("port", text),
            ("pathname", text),
            ("search", text),
        ],
    )
}

fn format_options(v: &Value, path: &str) -> Result<(), String> {
    fn encode(v: &Value, path: &str) -> Result<(), String> {
        object(
            v,
            path,
            true,
            &[],
            &[
                ("quality", leaf::<Option<u8>>),
                ("lossless", leaf::<bool>),
                ("effort", leaf::<Option<u8>>),
                ("progressive", leaf::<bool>),
            ],
        )
    }
    object(
        v,
        path,
        true,
        &["format"],
        &[("format", leaf::<ImageFormat>), ("options", encode)],
    )
}

fn image_size(v: &Value, path: &str) -> Result<(), String> {
    object(
        v,
        path,
        false,
        &["name"],
        &[
            ("name", leaf::<String>),
            ("width", leaf::<Option<u32>>),
            ("height", leaf::<Option<u32>>),
            ("fit", leaf::<Fit>),
            ("position", position),
            ("withoutEnlargement", leaf::<Option<bool>>),
            ("formatOptions", format_options),
            ("crop", position),
        ],
    )
}

fn limit_input_pixels(v: &Value, path: &str) -> Result<(), String> {
    match v {
        Value::Bool(false) => Err(format!(
            "{path}: the pixel cap can't be turned off; give a number of pixels"
        )),
        Value::Bool(true) => Err(format!(
            "{path}: `true` isn't needed; leave it out for the instance's cap, or give a number of pixels"
        )),
        // In sharp, 0 means no limit, the same as `false`.
        v if v.as_u64() == Some(0) => Err(format!(
            "{path}: 0 would turn the pixel cap off, which can't be done; give a number of pixels"
        )),
        v => leaf::<Option<u64>>(v, path),
    }
}

fn paste_url(v: &Value, path: &str) -> Result<(), String> {
    match v {
        Value::Bool(_) => Ok(()),
        Value::Object(_) => object(
            v,
            path,
            false,
            &["allowList"],
            &[("allowList", |v, p| list(v, p, url_pattern))],
        ),
        _ => Err(format!(
            "{path}: must be true, false or {{\"allowList\": [{{\"hostname\": \"example.com\"}}]}}"
        )),
    }
}

fn skip_safe_fetch(v: &Value, path: &str) -> Result<(), String> {
    match v {
        Value::Bool(_) => Ok(()),
        Value::Array(_) => list(v, path, url_pattern),
        _ => Err(format!(
            "{path}: must be true, false or a list like [{{\"hostname\": \"example.com\"}}]"
        )),
    }
}

/// Values parse would refuse with a bare serde message, and sharp-only values, named by
/// path instead. `upload` is the raw option.
pub(crate) fn check_raw(upload: &Value) -> Result<(), String> {
    match upload {
        Value::Null | Value::Bool(_) => return Ok(()),
        Value::Object(_) => {}
        _ => return Err("upload: must be true, false or an object of upload options".into()),
    }
    let flag: Check = leaf::<bool>;
    let text: Check = leaf::<Option<String>>;
    let texts: Check = leaf::<Option<Vec<String>>>;
    object(
        upload,
        "upload",
        false,
        &[],
        &[
            ("storage", leaf::<String>),
            ("mimeTypes", leaf::<Vec<String>>),
            ("filesizeLimit", leaf::<Option<u64>>),
            ("imageSizes", |v, p| list(v, p, image_size)),
            ("adminThumbnail", text),
            ("crop", flag),
            ("focalPoint", flag),
            ("formatOptions", format_options),
            ("resizeOptions", |v, p| {
                object(
                    v,
                    p,
                    true,
                    &[],
                    &[
                        ("width", leaf::<Option<u32>>),
                        ("height", leaf::<Option<u32>>),
                        ("fit", leaf::<Fit>),
                        ("position", position),
                        ("withoutEnlargement", leaf::<Option<bool>>),
                    ],
                )
            }),
            ("trimOptions", |v, p| {
                object(
                    v,
                    p,
                    true,
                    &[],
                    &[
                        ("background", leaf::<Option<String>>),
                        ("threshold", leaf::<Option<u8>>),
                    ],
                )
            }),
            ("withMetadata", flag),
            ("constructorOptions", |v, p| {
                object(
                    v,
                    p,
                    true,
                    &[],
                    &[
                        ("limitInputPixels", limit_input_pixels),
                        ("failOn", leaf::<FailOn>),
                    ],
                )
            }),
            ("staticDir", text),
            ("disableLocalStorage", flag),
            ("filesRequiredOnCreate", flag),
            ("filenameCompoundIndex", texts),
            ("pasteURL", paste_url),
            ("skipSafeFetch", skip_safe_fetch),
            ("allowRestrictedFileTypes", flag),
            ("bulkUpload", flag),
            ("displayPreview", flag),
            ("hideFileInputOnCreate", flag),
            ("hideRemoveFile", flag),
            ("cacheTags", flag),
            ("modifyResponseHeaders", |v, p| {
                let Value::Object(o) = v else {
                    return leaf::<BTreeMap<String, String>>(v, p);
                };
                for (name, value) in o {
                    leaf::<String>(value, &format!("{p}.{name}"))?;
                }
                Ok(())
            }),
        ],
    )?;
    // Anything the checks above missed still gets a path.
    Upload::deserialize(upload)
        .map(|_| ())
        .map_err(|e| format!("upload: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_made_safe() {
        let long = format!("{}.png", "é".repeat(150));
        for (raw, want) in [
            ("../../etc/passwd", "passwd"),
            ("..\\..\\boot.ini", "boot.ini"),
            (".htaccess", "htaccess"),
            ("CON.txt", "CON-file.txt"),
            ("lpt1", "lpt1-file"),
            ("con.tar.gz", "con-file.tar.gz"),
            ("aux .txt", "aux-file.txt"),
            ("a\u{202E}gnp.exe", "agnp.exe"),
            ("a\u{0}b\nc.png", "abc.png"),
            ("", "file"),
            ("..", "file"),
            ("photo.jpg", "photo.jpg"),
            ("COM0.txt", "COM0-file.txt"),
            ("lpt0", "lpt0-file"),
            ("COM\u{b9}.txt", "COM\u{b9}-file.txt"),
            ("lpt\u{b3}", "lpt\u{b3}-file"),
            ("CONIN$", "CONIN$-file"),
            ("conout$.log", "conout$-file.log"),
        ] {
            assert_eq!(sanitize_filename(raw), want, "{raw:?}");
        }
        let safe = sanitize_filename(&long);
        assert!(safe.len() <= 255 && safe.ends_with(".png"), "{safe}");
        assert!(safe.trim_end_matches(".png").chars().all(|c| c == 'é'));
        assert_eq!(numbered("a.png", 2), "a-2.png");
        assert_eq!(numbered("README", 1), "README-1");
        assert!(numbered(&safe, 12).len() <= 255);
        assert!(numbered(&safe, 12).ends_with("-12.png"));
        assert_eq!(size_filename("a.png", 640, 400, "webp"), "a-640x400.webp");
        // Never a leading dot, a trailing dot or space, or empty, whatever is cut.
        let ext = "x".repeat(300);
        let long_ext = format!("a.{ext}");
        assert_eq!(sanitize_filename(&long_ext), format!("a.{}", &ext[..32]));
        let spaced = format!("{} b", "a".repeat(254));
        assert_eq!(sanitize_filename(&spaced), "a".repeat(254));
        for made in [
            numbered(&long_ext, 2),
            size_filename(&long_ext, 1, 1, &ext),
            size_filename(".png", 1, 1, "webp"),
            numbered("...", 3),
            numbered(&spaced, 7),
        ] {
            assert!(
                !made.is_empty()
                    && made.len() <= 255
                    && !made.starts_with('.')
                    && !made.ends_with(['.', ' ']),
                "{made:?}"
            );
        }
        assert_eq!(size_filename(".png", 1, 1, "webp"), "png-1x1.webp");
    }

    #[test]
    fn mime_and_restricted() {
        let images = vec!["image/*".to_owned()];
        assert!(mime_allowed(&images, "image/png"));
        assert!(mime_allowed(&images, "IMAGE/PNG; charset=binary"));
        assert!(!mime_allowed(&images, "application/pdf"));
        assert!(mime_allowed(&[], "application/pdf"));
        assert!(mime_allowed(&["application/pdf".into()], "application/pdf"));
        assert!(mime_allowed(&["*/*".into()], "text/plain"));
        assert!(restricted("text/x-shellscript", "a.sh"));
        assert!(restricted("image/png", "a.php"));
        assert!(restricted("application/x-msdownload", "a.png"));
        assert!(restricted("image/png", "A.EXE"));
        assert!(!restricted("image/png", "a.png"));
        assert!(!restricted("image/png", "exe"));
    }
}
