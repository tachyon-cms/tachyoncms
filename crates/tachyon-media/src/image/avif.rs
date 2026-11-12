//! What the AVIF container says beyond what `avif-parse` reads: its brands, the frame
//! sizes its AV1 data can decode to, its rotation and mirroring, and where its EXIF and
//! XMP items sit, so they can be blanked without touching the picture.

use super::ImageError;

/// What an `ftyp` box says a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Brand {
    Avif,
    AnimatedAvif,
    Other,
}

/// The brands of the `ftyp` box at the start of `head`, if it has one.
pub(super) fn brand(head: &[u8]) -> Option<Brand> {
    if head.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let size = u32::from_be_bytes(head.get(..4)?.try_into().ok()?) as usize;
    let body = head.get(8..size.clamp(8, head.len()))?;
    // The major brand, the minor version (no brand), then the compatible brands.
    let all: Vec<&[u8; 4]> = body
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, b)| b)
        .collect();
    Some(if all.contains(&b"avif") {
        Brand::Avif
    } else if all.contains(&b"avis") {
        Brand::AnimatedAvif
    } else {
        Brand::Other
    })
}

/// `data` with `avif` as its major brand: `avif-parse` reads no other, though a file whose
/// compatible brands list `avif` (say, major `mif1`) is just as much an AVIF.
pub(super) fn as_avif(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    match data.get(8..12) {
        Some(b"avif") | None => std::borrow::Cow::Borrowed(data),
        Some(_) => {
            let mut out = data.to_vec();
            out[8..12].copy_from_slice(b"avif");
            std::borrow::Cow::Owned(out)
        }
    }
}

/// `work`, with a panic in it (avif-parse asserts on some malformed files in debug
/// builds; rav1d is a large port) taken as a corrupt file rather than a failed request.
pub(super) fn guarded<T>(
    work: impl FnOnce() -> Result<T, ImageError> + std::panic::UnwindSafe,
) -> Result<T, ImageError> {
    std::panic::catch_unwind(work)
        .unwrap_or_else(|_| Err(ImageError::Corrupt("this AVIF can't be parsed".into())))
}

/// `data` parsed by avif-parse, guarded.
pub(super) fn parse(data: &[u8]) -> Result<avif_parse::AvifData, ImageError> {
    guarded(|| avif_parse::read_avif(&mut &as_avif(data)[..]).map_err(super::avif_error))
}

/// The size one item's AV1 data decodes to, read from its one frame header.
///
/// Refused: an item with other than one sequence header (the decoder follows the last
/// one, so a first claiming a small frame could hide a huge one), other than one frame
/// (several could fill the decoder's eight reference slots), a frame that isn't a shown
/// key frame, or a decoder model, which no still picture needs. With a reduced still
/// picture header the frame is the header's maximum size; otherwise its own header says,
/// and the decoder doesn't hold that to the maximum.
pub(super) fn decode_bound(item: &[u8]) -> Result<(u32, u32), ImageError> {
    let (mut sequence, mut frames) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < item.len() {
        let header = item[i];
        let mut j = i + 1 + usize::from(header & 4 != 0);
        let end = if header & 2 != 0 {
            let (size, used) = leb128(item.get(j..).unwrap_or_default())?;
            j += used;
            j.checked_add(size).ok_or_else(bad_obu)?
        } else {
            item.len()
        };
        let body = item.get(j..end).ok_or_else(bad_obu)?;
        match (header >> 3) & 0xF {
            1 => sequence.push(body),
            3 | 6 => frames.push(body),
            _ => {}
        }
        i = end;
    }
    let [seq] = sequence[..] else {
        return Err(ImageError::Corrupt(format!(
            "an AVIF picture has {} sequence headers; it needs exactly one",
            sequence.len()
        )));
    };
    let [frame] = frames[..] else {
        return Err(ImageError::Corrupt(format!(
            "an AVIF picture has {} frames; a still has exactly one",
            frames.len()
        )));
    };
    let s = sequence_header(seq)?;
    frame_size(&s, frame)
}

fn bad_obu() -> ImageError {
    ImageError::Corrupt("an AVIF's AV1 data is cut off".into())
}

fn leb128(b: &[u8]) -> Result<(usize, usize), ImageError> {
    let mut value = 0u64;
    for (i, byte) in b.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((usize::try_from(value).map_err(|_| bad_obu())?, i + 1));
        }
    }
    Err(bad_obu())
}

/// What a frame header needs from the sequence header.
struct SequenceHeader {
    reduced: bool,
    decoder_model: bool,
    width_bits: u32,
    height_bits: u32,
    max_width: u32,
    max_height: u32,
    /// The bits of a frame id, 0 when frames carry none.
    id_bits: u32,
    /// 0, 1, or 2 for "each frame says".
    screen_content_tools: u32,
    integer_mv: u32,
    order_hint_bits: u32,
}

/// Bits, most significant first.
pub(super) struct Bits<'a> {
    pub data: &'a [u8],
    pub at: usize,
}

impl Bits<'_> {
    pub fn get(&mut self, n: u32) -> Result<u32, ImageError> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8).ok_or_else(bad_obu)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(v)
    }

    fn flag(&mut self) -> Result<bool, ImageError> {
        Ok(self.get(1)? == 1)
    }

    fn uvlc(&mut self) -> Result<(), ImageError> {
        let mut zeros = 0;
        while self.get(1)? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Ok(());
            }
        }
        self.get(zeros).map(|_| ())
    }
}

/// AV1 spec 5.5.1, as far as a frame header needs it.
fn sequence_header(data: &[u8]) -> Result<SequenceHeader, ImageError> {
    let mut b = Bits { data, at: 0 };
    let _profile = b.get(3)?;
    let _still = b.get(1)?;
    let reduced = b.flag()?;
    let mut decoder_model = false;
    if reduced {
        b.get(5)?;
    } else {
        let timing = b.flag()?;
        let mut delay_bits = 0;
        if timing {
            b.get(32)?;
            b.get(32)?;
            if b.flag()? {
                b.uvlc()?;
            }
            decoder_model = b.flag()?;
            if decoder_model {
                delay_bits = b.get(5)? + 1;
                b.get(32)?;
                b.get(5)?;
                b.get(5)?;
            }
        }
        let display_delay = b.flag()?;
        for _ in 0..=b.get(5)? {
            b.get(12)?;
            if b.get(5)? > 7 {
                b.get(1)?;
            }
            if decoder_model && b.flag()? {
                b.get(delay_bits)?;
                b.get(delay_bits)?;
                b.get(1)?;
            }
            if display_delay && b.flag()? {
                b.get(4)?;
            }
        }
    }
    let width_bits = b.get(4)? + 1;
    let height_bits = b.get(4)? + 1;
    let max_width = b.get(width_bits)? + 1;
    let max_height = b.get(height_bits)? + 1;
    let frame_ids = !reduced && b.flag()?;
    let id_bits = if frame_ids {
        let delta = b.get(4)? + 2;
        delta + b.get(3)? + 1
    } else {
        0
    };
    // 128×128 superblocks, filter intra, intra edge filter.
    b.get(3)?;
    let (screen_content_tools, integer_mv, order_hint_bits) = if reduced {
        (2, 2, 0)
    } else {
        // Interintra and masked compound, warped motion, dual filter.
        b.get(4)?;
        let order_hint = b.flag()?;
        if order_hint {
            b.get(2)?;
        }
        let tools = if b.flag()? { 2 } else { b.get(1)? };
        let mv = if tools > 0 {
            if b.flag()? { 2 } else { b.get(1)? }
        } else {
            2
        };
        let bits = if order_hint { b.get(3)? + 1 } else { 0 };
        (tools, mv, bits)
    };
    Ok(SequenceHeader {
        reduced,
        decoder_model,
        width_bits,
        height_bits,
        max_width,
        max_height,
        id_bits,
        screen_content_tools,
        integer_mv,
        order_hint_bits,
    })
}

/// AV1 spec 5.9.2, as far as `frame_size()`, for the one frame of a still.
fn frame_size(s: &SequenceHeader, frame: &[u8]) -> Result<(u32, u32), ImageError> {
    if s.reduced {
        return Ok((s.max_width, s.max_height));
    }
    let refuse = |why: &str| Err(ImageError::Corrupt(format!("an AVIF still {why}")));
    if s.decoder_model {
        return refuse("can't carry a decoder model");
    }
    let mut b = Bits { data: frame, at: 0 };
    if b.flag()? {
        return refuse("can't show an earlier frame");
    }
    let key = b.get(2)? == 0;
    let shown = b.flag()?;
    if !(key && shown) {
        return refuse("has to be a shown key frame");
    }
    // A shown key frame is error resilient; next is `disable_cdf_update`.
    b.get(1)?;
    let tools = if s.screen_content_tools == 2 {
        b.get(1)?
    } else {
        s.screen_content_tools
    };
    if tools == 1 && s.integer_mv == 2 {
        b.get(1)?;
    }
    b.get(s.id_bits)?;
    let own_size = b.flag()?;
    b.get(s.order_hint_bits)?;
    // A shown key frame refreshes every slot and needs no reference: its size is next.
    if own_size {
        Ok((b.get(s.width_bits)? + 1, b.get(s.height_bits)? + 1))
    } else {
        Ok((s.max_width, s.max_height))
    }
}

/// A box: its type, where its body starts and where it ends.
#[derive(Clone, Copy)]
struct Box4 {
    kind: [u8; 4],
    start: usize,
    body: usize,
    end: usize,
}

fn boxes(data: &[u8], from: usize, to: usize) -> Vec<Box4> {
    walk_boxes(data, from, to).0
}

/// The boxes from `from` to `to`, and whether they reach `to` exactly: a walk that stops
/// on a box it can't follow may have missed some.
fn walk_boxes(data: &[u8], from: usize, to: usize) -> (Vec<Box4>, bool) {
    let mut out = Vec::new();
    let to = to.min(data.len());
    let mut i = from;
    while let Some(h) = data.get(i..i + 8).filter(|_| i + 8 <= to) {
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind = [h[4], h[5], h[6], h[7]];
        let (body, size) = match size {
            0 => (i + 8, (to - i) as u64),
            1 => match data.get(i + 8..i + 16) {
                Some(l) => (i + 16, u64::from_be_bytes(l.try_into().unwrap_or_default())),
                None => break,
            },
            n => (i + 8, n),
        };
        let Some(end) = usize::try_from(size).ok().and_then(|s| i.checked_add(s)) else {
            break;
        };
        if end > to || end < body {
            break;
        }
        out.push(Box4 {
            kind,
            start: i,
            body,
            end,
        });
        i = end;
    }
    (out, i == to.min(data.len()))
}

fn find(list: &[Box4], kind: &[u8; 4]) -> Option<Box4> {
    list.iter().copied().find(|b| &b.kind == kind)
}

/// A big-endian number of `n` bytes at `*at`, inside `end`, moving past it.
fn read(data: &[u8], at: &mut usize, n: usize, end: usize) -> Option<u64> {
    let stop = at.checked_add(n).filter(|&s| s <= end)?;
    let bytes = data.get(*at..stop)?;
    *at = stop;
    Some(bytes.iter().fold(0u64, |v, &b| (v << 8) | u64::from(b)))
}

/// The most EXIF and XMP items an AVIF may carry, the most extents one item may have, and
/// the most extents `iloc` may list in all: past them the file is refused, so reading its
/// metadata stays linear.
const MAX_METADATA_ITEMS: usize = 64;
const MAX_ITEM_EXTENTS: u64 = 1024;
const MAX_EXTENTS: u64 = 65_536;

/// A clean aperture (`clap`): the crop, as fractions, centred then offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Clap {
    width: (u32, u32),
    height: (u32, u32),
    x: (i32, u32),
    y: (i32, u32),
}

impl Clap {
    /// The crop of a `w`×`h` picture: (x, y, width, height), or `None` if it's malformed or
    /// leaves it whole.
    pub fn rect(&self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let frac = |(n, d): (u32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
        let signed = |(n, d): (i32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
        let (cw, ch) = (frac(self.width)?, frac(self.height)?);
        let left = (f64::from(w) - cw) / 2.0 + signed(self.x)?;
        let top = (f64::from(h) - ch) / 2.0 + signed(self.y)?;
        let (x, y) = (left.round(), top.round());
        let (cw, ch) = (cw.round(), ch.round());
        let fits = x >= 0.0 && y >= 0.0 && cw >= 1.0 && ch >= 1.0;
        if !fits || x + cw > f64::from(w) || y + ch > f64::from(h) {
            return None;
        }
        let r = (x as u32, y as u32, cw as u32, ch as u32);
        (r != (0, 0, w, h)).then_some(r)
    }
}

/// An EXIF or XMP item: which, and its byte ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub exif: bool,
    pub ranges: Vec<(usize, usize)>,
}

/// What `meta` says about the primary item and the metadata items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Meta {
    /// The EXIF orientation that does what the primary item's `irot` and `imir` do.
    pub orientation: u8,
    pub clap: Option<Clap>,
    /// The bytes of the primary item's ICC profile (`colr` of type `prof` or `rICC`).
    pub icc: Option<(usize, usize)>,
    pub items: Vec<Item>,
    /// An EXIF or XMP item whose bytes couldn't be found: blanking can't be trusted, so
    /// the file is re-encoded instead.
    pub unresolved: bool,
}

impl Meta {
    /// The EXIF (as TIFF, the item's offset prefix gone) and XMP these items hold.
    pub fn exif_and_xmp(&self, data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let join = |i: &Item| -> Vec<u8> {
            i.ranges
                .iter()
                .flat_map(|&(s, e)| data.get(s..e).unwrap_or_default().iter().copied())
                .collect()
        };
        let exif = self.items.iter().find(|i| i.exif).map(join).and_then(|b| {
            let skip = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize;
            b.get(4 + skip..).map(<[u8]>::to_vec)
        });
        let xmp = self.items.iter().find(|i| !i.exif).map(join);
        (exif, xmp)
    }
}

/// Reads `meta` from the AVIF `data`. Every read is checked; what it can't follow is left
/// as found, except metadata it can't locate, which sets `unresolved`.
pub(super) fn meta(data: &[u8]) -> Result<Meta, ImageError> {
    let mut m = Meta {
        orientation: 1,
        ..Meta::default()
    };
    // A `meta` past where the walk stopped, or a second one, would go unseen: fail closed.
    let (top, whole) = walk_boxes(data, 0, data.len());
    m.unresolved = !whole || top.iter().filter(|b| &b.kind == b"meta").count() > 1;
    let Some(meta) = find(&top, b"meta") else {
        return Ok(m);
    };
    let (inner, whole) = walk_boxes(data, meta.body + 4, meta.end);
    // A metadata item past where the walk stopped would go unseen: fail closed.
    m.unresolved |= !whole;
    let primary = find(&inner, b"pitm").and_then(|b| {
        let wide = if *data.get(b.body)? == 0 { 2 } else { 4 };
        read(data, &mut (b.body + 4), wide, b.end)
    });
    let (wanted, seen_all) = metadata_items(data, &inner)?;
    m.unresolved |= !seen_all;
    let located = locations(data, &inner, &wanted, None)?;
    for (id, exif) in wanted {
        match located.iter().find(|(i, _)| *i == id) {
            Some((_, Some(ranges))) => m.items.push(Item {
                exif,
                ranges: ranges.clone(),
            }),
            _ => m.unresolved = true,
        }
    }
    if let Some(p) = primary.and_then(|id| properties(data, &inner, id)) {
        m.orientation = orientation_of(&p.ops);
        m.clap = p.clap;
        m.icc = p.icc;
    }
    Ok(m)
}

/// The `Exif` items and the `mime` items holding XMP: (id, is EXIF), and whether every
/// item `iinf` counts was seen.
type Wanted = (Vec<(u64, bool)>, bool);

fn metadata_items(data: &[u8], inner: &[Box4]) -> Result<Wanted, ImageError> {
    let Some(iinf) = find(inner, b"iinf") else {
        return Ok((Vec::new(), true));
    };
    let Some(&version) = data.get(iinf.body) else {
        return Ok((Vec::new(), false));
    };
    let count_bytes = if version == 0 { 2 } else { 4 };
    let Some(count) = read(data, &mut (iinf.body + 4), count_bytes, iinf.end) else {
        return Ok((Vec::new(), false));
    };
    let (entries, whole) = walk_boxes(data, iinf.body + 4 + count_bytes, iinf.end);
    let infes: Vec<&Box4> = entries.iter().filter(|b| &b.kind == b"infe").collect();
    let seen_all = whole && infes.len() as u64 == count;
    let mut out = Vec::new();
    for infe in infes {
        let Some(&version) = data.get(infe.body) else {
            continue;
        };
        if version < 2 {
            continue;
        }
        let mut at = infe.body + 4;
        let id_bytes = if version == 2 { 2 } else { 4 };
        let Some(id) = read(data, &mut at, id_bytes, infe.end) else {
            continue;
        };
        at += 2;
        let Some(kind) = data.get(at..at + 4).filter(|_| at + 4 <= infe.end) else {
            continue;
        };
        at += 4;
        let rest = data.get(at..infe.end).unwrap_or_default();
        // The name, then for `mime` the content type, each ending in a zero.
        let mut strings = rest.split(|&b| b == 0);
        let _name = strings.next();
        let content = strings.next().unwrap_or_default().to_ascii_lowercase();
        let xmp = kind == b"mime"
            && (content.windows(3).any(|w| w == b"xmp") || content.ends_with(b"rdf+xml"));
        if kind == b"Exif" || xmp {
            out.push((id, kind == b"Exif"));
            if out.len() > MAX_METADATA_ITEMS {
                return Err(ImageError::Corrupt(
                    "this AVIF carries too many metadata items".into(),
                ));
            }
        }
    }
    Ok((out, seen_all))
}

/// An item id and its byte ranges, `None` when they can't be found.
type Located = (u64, Option<Vec<(usize, usize)>>);

/// The file ranges `iloc` gives each of `wanted`, from one pass over it: `None` for an
/// item whose ranges can't be found (construction method 2, no `idat`, out of range).
/// With `all`, every item's: an extent of length 0 ("to the end of the data") ends where
/// `all` says the box holding its start does, and an `iloc` that can't be read through is
/// an item `(0, None)`.
fn locations(
    data: &[u8],
    inner: &[Box4],
    wanted: &[(u64, bool)],
    all: Option<&dyn Fn(usize) -> Option<usize>>,
) -> Result<Vec<Located>, ImageError> {
    let mut out = Vec::new();
    let unread = || {
        if all.is_some() {
            vec![(0, None)]
        } else {
            Vec::new()
        }
    };
    if wanted.is_empty() && all.is_none() {
        return Ok(out);
    }
    let Some(iloc) = find(inner, b"iloc") else {
        return Ok(unread());
    };
    let idat = find(inner, b"idat").map(|b| b.body as u64);
    let end = iloc.end;
    let mut at = iloc.body;
    let cut = || Ok(unread());
    let Some(version) = read(data, &mut at, 1, end) else {
        return cut();
    };
    at += 3;
    let (Some(sizes), Some(more)) = (read(data, &mut at, 1, end), read(data, &mut at, 1, end))
    else {
        return cut();
    };
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 15) as usize);
    let base_size = (more >> 4) as usize;
    let index_size = if version >= 1 {
        (more & 15) as usize
    } else {
        0
    };
    let wide = if version < 2 { 2 } else { 4 };
    let Some(count) = read(data, &mut at, wide, end) else {
        return cut();
    };
    let mut total = 0u64;
    for _ in 0..count {
        let item = (|| {
            let id = read(data, &mut at, wide, end)?;
            let method = if version >= 1 {
                read(data, &mut at, 2, end)? & 15
            } else {
                0
            };
            read(data, &mut at, 2, end)?;
            let base = read(data, &mut at, base_size, end)?;
            let n = read(data, &mut at, 2, end)?;
            Some((id, method, base, n))
        })();
        let Some((id, method, base, n)) = item else {
            out.extend(unread());
            break;
        };
        total += n;
        if n > MAX_ITEM_EXTENTS || total > MAX_EXTENTS {
            return Err(ImageError::Corrupt(
                "this AVIF lists too many extents".into(),
            ));
        }
        let mut ranges = Some(Vec::new());
        for _ in 0..n {
            let extent = (|| {
                read(data, &mut at, index_size, end)?;
                let off = read(data, &mut at, offset_size, end)?;
                let len = read(data, &mut at, length_size, end)?;
                Some((off, len))
            })();
            let Some((off, len)) = extent else {
                ranges = None;
                break;
            };
            // Offsets are into the file, or with method 1 into `idat`.
            let origin = match method {
                0 => Some(0),
                1 => idat,
                _ => None,
            };
            let range = origin.and_then(|o| {
                let start = usize::try_from(o.checked_add(base)?.checked_add(off)?).ok()?;
                let stop = if len == 0 {
                    match all {
                        Some(box_end) => box_end(start)?,
                        None => data.len(),
                    }
                } else {
                    start.checked_add(usize::try_from(len).ok()?)?
                };
                (start <= stop && stop <= data.len()).then_some((start, stop))
            });
            match (range, ranges.as_mut()) {
                (Some(r), Some(list)) => list.push(r),
                _ => ranges = None,
            }
        }
        if all.is_some() || wanted.iter().any(|(w, _)| *w == id) {
            out.push((id, ranges));
        }
    }
    Ok(out)
}

/// One of the primary item's transformative properties, in the order they apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `irot`: this many quarter turns anticlockwise.
    Rotate(u8),
    /// `imir`: 0 mirrors about a vertical axis (left and right swap), 1 a horizontal one.
    Mirror(u8),
}

struct Properties {
    ops: Vec<Op>,
    clap: Option<Clap>,
    icc: Option<(usize, usize)>,
}

fn properties(data: &[u8], inner: &[Box4], item: u64) -> Option<Properties> {
    let iprp = find(inner, b"iprp")?;
    let props = boxes(data, iprp.body, iprp.end);
    let ipco = find(&props, b"ipco")?;
    let list = boxes(data, ipco.body, ipco.end);
    let ipma = find(&props, b"ipma")?;
    let end = ipma.end;
    let version = *data.get(ipma.body)?;
    let wide_index = *data.get(ipma.body + 3)? & 1 == 1;
    let mut at = ipma.body + 4;
    let entries = read(data, &mut at, 4, end)?;
    for _ in 0..entries {
        let id = read(data, &mut at, if version < 1 { 2 } else { 4 }, end)?;
        let n = read(data, &mut at, 1, end)?;
        let mut indices = Vec::new();
        for _ in 0..n {
            let index = if wide_index {
                read(data, &mut at, 2, end)? & 0x7FFF
            } else {
                read(data, &mut at, 1, end)? & 0x7F
            };
            indices.push(index);
        }
        if id != item {
            continue;
        }
        let mut p = Properties {
            ops: Vec::new(),
            clap: None,
            icc: None,
        };
        let u32_at = |i: usize| -> Option<u32> {
            Some(u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?))
        };
        for b in indices
            .into_iter()
            .filter_map(|i| list.get(usize::try_from(i).ok()?.checked_sub(1)?))
        {
            match &b.kind {
                b"irot" => p.ops.push(Op::Rotate(data.get(b.body)? & 3)),
                b"imir" => p.ops.push(Op::Mirror(data.get(b.body)? & 1)),
                b"clap" if b.end >= b.body + 32 => {
                    let v = |k: usize| u32_at(b.body + 4 * k);
                    p.clap = Some(Clap {
                        width: (v(0)?, v(1)?),
                        height: (v(2)?, v(3)?),
                        x: (v(4)? as i32, v(5)?),
                        y: (v(6)? as i32, v(7)?),
                    });
                }
                b"colr" => {
                    let kind = data.get(b.body..b.body + 4)?;
                    if kind == b"prof" || kind == b"rICC" {
                        p.icc = Some((b.body + 4, b.end));
                    }
                }
                _ => {}
            }
        }
        return Some(p);
    }
    None
}

/// The EXIF orientation doing what `ops` do, found by doing both to a small picture
/// whose pixels are all different.
fn orientation_of(ops: &[Op]) -> u8 {
    use ::image::{DynamicImage, GrayImage, Luma, metadata::Orientation};
    let probe =
        DynamicImage::ImageLuma8(GrayImage::from_fn(3, 2, |x, y| Luma([(y * 3 + x) as u8])));
    let mut done = probe.clone();
    for op in ops {
        done = match op {
            Op::Rotate(1) => done.rotate270(),
            Op::Rotate(2) => done.rotate180(),
            Op::Rotate(3) => done.rotate90(),
            Op::Rotate(_) => done,
            Op::Mirror(0) => done.fliph(),
            Op::Mirror(_) => done.flipv(),
        };
    }
    (1..=8u8)
        .find(|&o| {
            let mut candidate = probe.clone();
            if let Some(o) = Orientation::from_exif(o) {
                candidate.apply_orientation(o);
            }
            candidate == done
        })
        .unwrap_or(1)
}

/// The `meta` children whose bodies an AVIF's picture is read from: the rest (`free`,
/// `skip`, anything unknown) keep only their headers.
const META_KEEP: [&[u8; 4]; 9] = [
    b"hdlr", b"pitm", b"iloc", b"iinf", b"iref", b"iprp", b"idat", b"dinf", b"grpl",
];

/// `data` with nothing but what its picture is made of, each byte where it was so every
/// offset still holds: `ftyp`; `meta`'s header and the bodies of its `META_KEEP` children
/// (the headers of the others); the bytes of the primary item, the tiles it's made of and
/// its alpha (and the Exif and XMP items with `keep_metadata`); and the headers of the
/// top-level boxes these sit in. Every other byte is zeroed, other items' included, and the
/// file ends after the furthest kept one, the box holding that end shortened to it. `None`
/// when that end can't be known: no `ftyp`, `meta` or primary item, an item kept that
/// can't be located, or an end inside a box the walk can't reach.
pub(super) fn keep(data: &[u8], keep_metadata: bool) -> Option<Vec<u8>> {
    // The top-level boxes: (start, header length, end, type, whether its size is "to the
    // end"), up to one that overruns the file.
    let mut top = Vec::new();
    let mut i = 0;
    while let Some(h) = data.get(i..i + 8) {
        let field = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        let (size, header) = match field {
            0 => ((data.len() - i) as u64, 8),
            1 => (
                u64::from_be_bytes(data.get(i + 8..i + 16)?.try_into().ok()?),
                16,
            ),
            n => (u64::from(n), 8),
        };
        let end = usize::try_from(size).ok().and_then(|s| i.checked_add(s));
        match end {
            Some(end) if size >= header as u64 && end <= data.len() => {
                top.push((i, header, end, [h[4], h[5], h[6], h[7]], field == 0));
                i = end;
            }
            _ => break,
        }
    }
    let whole = |kind: &[u8; 4]| top.iter().find(|b| &b.3 == kind).copied();
    let ftyp = whole(b"ftyp")?;
    let (meta, header, meta_end, _, _) = whole(b"meta")?;
    // `meta` is a full box: its children follow a version and flags.
    let body = meta + header + 4;
    let inner = boxes(data, body, meta_end);
    let mut kept = vec![(ftyp.0, ftyp.2), (meta, body)];
    for b in &inner {
        kept.push((b.start, b.body));
        if META_KEEP.contains(&&b.kind) {
            kept.push((b.body, b.end));
        }
    }
    let primary = find(&inner, b"pitm").and_then(|b| {
        let wide = if *data.get(b.body)? == 0 { 2 } else { 4 };
        read(data, &mut (b.body + 4), wide, b.end)
    })?;
    let mut items = vec![primary];
    for (kind, from, to) in references(data, &inner) {
        match &kind {
            b"dimg" if from == primary => items.extend(to),
            b"auxl" if to.contains(&primary) => items.push(from),
            _ => {}
        }
    }
    if keep_metadata {
        items.extend(
            metadata_items(data, &inner)
                .ok()?
                .0
                .into_iter()
                .map(|(id, _)| id),
        );
    }
    // An extent of length 0 runs to the end of the box it starts in, when that has one.
    let box_end = |at: usize| {
        top.iter()
            .find(|b| b.0 <= at && at < b.2 && !b.4)
            .map(|b| b.2)
    };
    let located = locations(data, &inner, &[], Some(&box_end)).ok()?;
    for id in items {
        let (_, ranges) = located.iter().find(|(i, _)| *i == id)?;
        kept.extend(ranges.clone()?);
    }
    let cut = kept.iter().map(|r| r.1).max()?;
    let mut out = vec![0u8; cut];
    for &(s, e) in &kept {
        out[s..e].copy_from_slice(&data[s..e]);
    }
    let mut reached = false;
    for &(start, header, end, _, _) in top.iter().filter(|b| b.0 < cut) {
        if start + header > cut {
            return None;
        }
        out[start..start + header].copy_from_slice(&data[start..start + header]);
        if end >= cut {
            reached = true;
            let size = (cut - start) as u64;
            if header == 16 {
                out[start + 8..start + 16].copy_from_slice(&size.to_be_bytes());
            } else {
                out[start..start + 4].copy_from_slice(&u32::try_from(size).ok()?.to_be_bytes());
            }
        }
    }
    reached.then_some(out)
}

/// The item references in `iref`: (type, from, to), each read within its box.
fn references(data: &[u8], inner: &[Box4]) -> Vec<([u8; 4], u64, Vec<u64>)> {
    let Some(iref) = find(inner, b"iref") else {
        return Vec::new();
    };
    let Some(&version) = data.get(iref.body) else {
        return Vec::new();
    };
    let wide = if version == 0 { 2 } else { 4 };
    boxes(data, iref.body + 4, iref.end)
        .into_iter()
        .filter_map(|r| {
            let mut at = r.body;
            let from = read(data, &mut at, wide, r.end)?;
            let n = read(data, &mut at, 2, r.end)?;
            let to = (0..n)
                .map(|_| read(data, &mut at, wide, r.end))
                .collect::<Option<Vec<_>>>()?;
            Some((r.kind, from, to))
        })
        .collect()
}

/// `data` with the EXIF and XMP items blanked, or `None` when it has none. The caller
/// re-encodes instead when `m.unresolved`.
pub(super) fn strip(data: &[u8], m: &Meta) -> Option<Vec<u8>> {
    let ranges = || m.items.iter().flat_map(|i| i.ranges.iter().copied());
    if ranges().all(|(s, e)| data.get(s..e).is_none_or(|b| b.iter().all(|&x| x == 0))) {
        return None;
    }
    let mut out = data.to_vec();
    for (start, end) in ranges() {
        if let Some(b) = out.get_mut(start..end) {
            b.fill(0);
        }
    }
    Some(out)
}

/// `data` with `icc` added to its primary item as a `colr` of type `prof`, for a file
/// ravif made (which writes no profile): the box goes at the end of `ipco`, its index on
/// the primary item's `ipma` entry, and every `iloc` offset past it moves along. `None`
/// when the file isn't laid out the way that needs.
pub(super) fn with_icc(data: &[u8], icc: &[u8]) -> Option<Vec<u8>> {
    let top = boxes(data, 0, data.len());
    let meta = find(&top, b"meta")?;
    let inner = boxes(data, meta.body + 4, meta.end);
    let iprp = find(&inner, b"iprp")?;
    let props = boxes(data, iprp.body, iprp.end);
    let ipco = find(&props, b"ipco")?;
    let ipma = find(&props, b"ipma")?;
    let iloc = find(&inner, b"iloc")?;
    let mdat = find(&top, b"mdat")?;
    // ravif's layout: one-byte indices, two-byte ids, the primary item first in `ipma`,
    // 4-byte iloc offsets with no base, version 0, and `mdat` after `meta`.
    let ipma_ok = *data.get(ipma.body)? == 0 && *data.get(ipma.body + 3)? & 1 == 0;
    let iloc_ok = *data.get(iloc.body)? == 0
        && *data.get(iloc.body + 4)? >> 4 == 4
        && *data.get(iloc.body + 5)? >> 4 == 0;
    // The primary item must be the first `ipma` entry, the one the index goes on.
    let pitm = find(&inner, b"pitm")?;
    if *data.get(pitm.body)? != 0 {
        return None;
    }
    let primary = read(data, &mut (pitm.body + 4), 2, pitm.end)?;
    let first = read(data, &mut (ipma.body + 8), 2, ipma.end)?;
    if !ipma_ok || !iloc_ok || mdat.body < meta.end || primary != first {
        return None;
    }
    let count = boxes(data, ipco.body, ipco.end).len();
    let index = u8::try_from(count + 1).ok().filter(|i| *i < 0x80)?;
    let mut colr = u32::try_from(icc.len() + 12).ok()?.to_be_bytes().to_vec();
    colr.extend_from_slice(b"colrprof");
    colr.extend_from_slice(icc);
    let n_at = ipma.body + 8 + 2;
    let n = usize::from(*data.get(n_at)?);
    let grow_by = colr.len() + 1;
    let mut out = data.to_vec();
    // Later edits first, so earlier offsets stay right.
    out.insert(n_at + 1 + n, 0x80 | index);
    out[n_at] = u8::try_from(n + 1).ok()?;
    let ipco_end = ipco.end;
    out.splice(ipco_end..ipco_end, colr.iter().copied());
    let grow = |out: &mut Vec<u8>, at: usize, by: usize| -> Option<()> {
        let size = u32::from_be_bytes(out.get(at..at + 4)?.try_into().ok()?);
        let size = size.checked_add(u32::try_from(by).ok()?)?;
        out[at..at + 4].copy_from_slice(&size.to_be_bytes());
        Some(())
    };
    // ipma and ipco come after the iloc fields being moved only if iloc follows iprp.
    grow(&mut out, ipma.body - 8 + colr.len(), 1)?;
    grow(&mut out, ipco.body - 8, colr.len())?;
    grow(&mut out, iprp.body - 8, grow_by)?;
    grow(&mut out, meta.body - 8, grow_by)?;
    // Every extent in `mdat` moves by what was added.
    let iloc_at = if iloc.body > ipco_end {
        iloc.body + grow_by
    } else {
        iloc.body
    };
    let length_size = usize::from(out[iloc_at + 4] & 15);
    let mut at = iloc_at + 6;
    let items = read(&out, &mut at, 2, out.len())?;
    for _ in 0..items {
        at += 2 + 2;
        let extents = read(&out, &mut at, 2, out.len())?;
        for _ in 0..extents {
            let off = u32::try_from(read(&out, &mut at, 4, out.len())?).ok()?;
            let moved = off.checked_add(u32::try_from(grow_by).ok()?)?;
            out[at - 4..at].copy_from_slice(&moved.to_be_bytes());
            at += length_size;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_compose_into_an_orientation() {
        assert_eq!(orientation_of(&[]), 1);
        // A quarter turn anticlockwise is EXIF's 8.
        assert_eq!(orientation_of(&[Op::Rotate(1)]), 8);
        assert_eq!(orientation_of(&[Op::Rotate(2)]), 3);
        assert_eq!(orientation_of(&[Op::Rotate(3)]), 6);
        assert_eq!(orientation_of(&[Op::Mirror(0)]), 2);
        assert_eq!(orientation_of(&[Op::Mirror(1)]), 4);
    }

    #[test]
    fn brands_are_read() {
        let ftyp = |major: &[u8], compat: &[&[u8]]| {
            let mut b = Vec::new();
            b.extend_from_slice(&((16 + 4 * compat.len()) as u32).to_be_bytes());
            b.extend_from_slice(b"ftyp");
            b.extend_from_slice(major);
            b.extend_from_slice(&[0; 4]);
            for c in compat {
                b.extend_from_slice(c);
            }
            b
        };
        assert_eq!(
            brand(&ftyp(b"mif1", &[b"mif1", b"avif"])),
            Some(Brand::Avif)
        );
        assert_eq!(
            brand(&ftyp(b"avis", &[b"avis", b"msf1"])),
            Some(Brand::AnimatedAvif)
        );
        assert_eq!(
            brand(&ftyp(b"heic", &[b"mif1", b"heic"])),
            Some(Brand::Other)
        );
        assert_eq!(brand(b"\x89PNG\r\n\x1a\n"), None);
    }

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A file of just `meta`: an `Exif` item 2 whose iloc entry (version 1, 4-byte
    /// offsets and lengths) has `extents` and construction `method`, then 64 bytes of data.
    fn synthetic(method: u8, extents: &[(u32, u32)]) -> Vec<u8> {
        let mut infe = vec![2, 0, 0, 0, 0, 2, 0, 0];
        infe.extend_from_slice(b"Exif\0");
        let mut iinf = vec![0, 0, 0, 0, 0, 1];
        iinf.extend(bx(b"infe", &infe));
        let mut iloc = vec![1, 0, 0, 0, 0x44, 0x00, 0, 1, 0, 2, 0, method, 0, 0];
        iloc.extend_from_slice(&(extents.len() as u16).to_be_bytes());
        for (o, l) in extents {
            iloc.extend_from_slice(&o.to_be_bytes());
            iloc.extend_from_slice(&l.to_be_bytes());
        }
        let mut meta = vec![0, 0, 0, 0];
        meta.extend(bx(b"pitm", &[0, 0, 0, 0, 0, 1]));
        meta.extend(bx(b"iinf", &iinf));
        meta.extend(bx(b"iloc", &iloc));
        let mut out = bx(b"meta", &meta);
        out.extend(bx(b"mdat", &[0x55; 64]));
        out
    }

    #[test]
    fn a_top_level_walk_that_stops_early_is_unresolved() {
        let good = synthetic(0, &[(10, 4)]);
        assert!(!meta(&good).unwrap().unresolved);
        // A box claiming more bytes than the file has, before `meta`.
        let mut early = bx(b"free", &[]);
        early[3] = 99;
        early.extend_from_slice(&good);
        assert!(meta(&early).unwrap().unresolved);
        // Trailing bytes that don't walk as a box.
        let mut tail = good.clone();
        tail.extend_from_slice(&[0, 0, 0, 99, b'f', b'r', b'e', b'e']);
        assert!(meta(&tail).unwrap().unresolved);
        // A second `meta`.
        let mut twice = good.clone();
        twice.extend(bx(b"meta", &[0; 4]));
        assert!(meta(&twice).unwrap().unresolved);
    }

    #[test]
    fn an_extent_past_the_end_doesnt_panic() {
        let mut data = synthetic(0, &[(0, 0)]);
        let past = (data.len() + 100) as u32;
        data = synthetic(0, &[(past, 0)]);
        if let Ok(m) = meta(&data) {
            let _ = strip(&data, &m);
        }
    }

    #[test]
    fn too_many_extents_are_refused() {
        let extents: Vec<(u32, u32)> = (0..2000).map(|i| (i, 1)).collect();
        assert!(meta(&synthetic(0, &extents)).is_err());
    }

    /// Bits written most significant first.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        at: usize,
    }

    impl Writer {
        fn put(&mut self, v: u32, n: u32) -> &mut Self {
            for i in (0..n).rev() {
                if self.at.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.at % 8);
                self.at += 1;
            }
            self
        }
    }

    /// A sequence header that isn't reduced: 16-bit sizes up to `max`, no timing, one
    /// operating point, frame tools chosen per frame, no order hints.
    fn sequence(max: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.put(0, 3).put(1, 1).put(0, 1); // profile, still, not reduced
        w.put(0, 1).put(0, 1); // no timing info, no display delay
        w.put(0, 5).put(0, 12).put(0, 5); // one operating point, level 0
        w.put(15, 4).put(15, 4).put(max - 1, 16).put(max - 1, 16);
        w.put(0, 1); // no frame ids
        w.put(0, 3).put(0, 4); // superblock, filter-intra, edge; compound tools
        w.put(0, 1); // no order hints
        w.put(1, 1); // screen content tools chosen per frame
        w.put(1, 1); // integer mv chosen per frame
        w.bytes
    }

    #[test]
    fn a_frame_header_gives_its_own_size() {
        let s = sequence_header(&sequence(1000)).unwrap();
        assert!(!s.reduced);
        let mut f = Writer::default();
        f.put(0, 1).put(0, 2).put(1, 1); // not an earlier frame; key; shown
        f.put(0, 1); // cdf updates
        f.put(1, 1).put(0, 1); // screen content tools, then integer mv
        f.put(1, 1).put(99, 16).put(49, 16); // its own size: 100×50
        assert_eq!(frame_size(&s, &f.bytes).unwrap(), (100, 50));
        let mut f = Writer::default();
        f.put(0, 1)
            .put(0, 2)
            .put(1, 1)
            .put(0, 1)
            .put(0, 1)
            .put(0, 1);
        assert_eq!(frame_size(&s, &f.bytes).unwrap(), (1000, 1000));
        let mut f = Writer::default();
        f.put(0, 1).put(1, 2).put(1, 1); // an inter frame
        assert!(frame_size(&s, &f.bytes).is_err());
    }

    #[test]
    fn unlocatable_metadata_is_flagged() {
        assert!(meta(&synthetic(2, &[(0, 10)])).unwrap().unresolved);
        assert!(
            meta(&synthetic(1, &[(0, 10)])).unwrap().unresolved,
            "no idat"
        );
        let good = synthetic(0, &[(10, 4)]);
        let m = meta(&good).unwrap();
        assert!(!m.unresolved);
        assert_eq!(m.items[0].ranges, vec![(10, 14)]);
    }

    #[test]
    fn a_short_iinf_count_is_unresolved() {
        let mut data = synthetic(0, &[(10, 4)]);
        // iinf says 2 entries; it holds 1.
        let at = data.windows(4).position(|w| w == b"iinf").unwrap() + 4 + 4;
        data[at + 1] = 2;
        assert!(meta(&data).unwrap().unresolved);
    }
}
