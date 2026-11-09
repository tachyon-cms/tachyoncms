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

/// The pixels the AV1 data of one item can make the decoder allocate. Refuses an item with
/// other than one sequence header: the decoder follows the last one it sees, so a first
/// header claiming a small frame could hide a huge one.
pub(super) fn decode_bound(item: &[u8]) -> Result<(u32, u32, u64), ImageError> {
    let mut headers = Vec::new();
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
        if (header >> 3) & 0xF == 1 {
            headers.push(sequence_header(body)?);
        }
        i = end;
    }
    let [s] = &headers[..] else {
        return Err(ImageError::Corrupt(format!(
            "an AVIF picture has {} sequence headers; it needs exactly one",
            headers.len()
        )));
    };
    // With a reduced header every frame is the maximum size. Otherwise a frame may set
    // its own size in as many bits as the header gives, and the decoder doesn't hold it
    // to the maximum, so that's the bound.
    let bound = if s.reduced {
        u64::from(s.max_width) * u64::from(s.max_height)
    } else {
        (1u64 << s.width_bits) * (1u64 << s.height_bits)
    };
    Ok((s.max_width, s.max_height, bound))
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

struct SequenceHeader {
    reduced: bool,
    width_bits: u32,
    height_bits: u32,
    max_width: u32,
    max_height: u32,
}

/// Bits, most significant first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn get(&mut self, n: u32) -> Result<u32, ImageError> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8).ok_or_else(bad_obu)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(v)
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

/// AV1 spec 5.5.1, as far as the frame size.
fn sequence_header(data: &[u8]) -> Result<SequenceHeader, ImageError> {
    let mut b = Bits { data, at: 0 };
    let _profile = b.get(3)?;
    let _still = b.get(1)?;
    let reduced = b.get(1)? == 1;
    if reduced {
        b.get(5)?;
    } else {
        let timing = b.get(1)? == 1;
        let mut decoder_model = false;
        let mut delay_bits = 0;
        if timing {
            b.get(32)?;
            b.get(32)?;
            if b.get(1)? == 1 {
                b.uvlc()?;
            }
            decoder_model = b.get(1)? == 1;
            if decoder_model {
                delay_bits = b.get(5)? + 1;
                b.get(32)?;
                b.get(5)?;
                b.get(5)?;
            }
        }
        let display_delay = b.get(1)? == 1;
        for _ in 0..=b.get(5)? {
            b.get(12)?;
            if b.get(5)? > 7 {
                b.get(1)?;
            }
            if decoder_model && b.get(1)? == 1 {
                b.get(delay_bits)?;
                b.get(delay_bits)?;
                b.get(1)?;
            }
            if display_delay && b.get(1)? == 1 {
                b.get(4)?;
            }
        }
    }
    let width_bits = b.get(4)? + 1;
    let height_bits = b.get(4)? + 1;
    let max_width = b.get(width_bits)? + 1;
    let max_height = b.get(height_bits)? + 1;
    Ok(SequenceHeader {
        reduced,
        width_bits,
        height_bits,
        max_width,
        max_height,
    })
}

/// A box: its type, where its body starts and where it ends.
#[derive(Clone, Copy)]
struct Box4 {
    kind: [u8; 4],
    body: usize,
    end: usize,
}

fn boxes(data: &[u8], from: usize, to: usize) -> Vec<Box4> {
    let mut out = Vec::new();
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
        out.push(Box4 { kind, body, end });
        i = end;
    }
    out
}

fn find(list: &[Box4], kind: &[u8; 4]) -> Option<Box4> {
    list.iter().copied().find(|b| &b.kind == kind)
}

/// A big-endian number of `n` bytes at `*at`, moving past it.
fn read(data: &[u8], at: &mut usize, n: usize) -> Option<u64> {
    let bytes = data.get(*at..*at + n)?;
    *at += n;
    Some(bytes.iter().fold(0u64, |v, &b| (v << 8) | u64::from(b)))
}

/// What `meta` says about the primary item and the metadata items.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Meta {
    /// The EXIF orientation that does what the primary item's `irot` and `imir` do.
    pub orientation: u8,
    /// The byte ranges of the EXIF and XMP items.
    pub metadata: Vec<(usize, usize)>,
}

/// Reads `meta` from the AVIF `data`. Anything it can't follow is left as found:
/// `avif-parse` has already checked the structure the picture needs.
pub(super) fn meta(data: &[u8]) -> Meta {
    let top = boxes(data, 0, data.len());
    let Some(meta) = find(&top, b"meta") else {
        return Meta {
            orientation: 1,
            ..Meta::default()
        };
    };
    let inner = boxes(data, meta.body + 4, meta.end);
    let primary = find(&inner, b"pitm").and_then(|b| {
        let mut at = b.body + 4;
        read(data, &mut at, if data[b.body] == 0 { 2 } else { 4 })
    });
    let orientation = primary
        .and_then(|item| transforms(data, &inner, item))
        .map_or(1, |ops| orientation_of(&ops));
    let mut metadata = Vec::new();
    for item in metadata_items(data, &inner) {
        metadata.extend(extents(data, &inner, item));
    }
    Meta {
        orientation,
        metadata,
    }
}

/// The `Exif` items and the `mime` items holding XMP.
fn metadata_items(data: &[u8], inner: &[Box4]) -> Vec<u64> {
    let Some(iinf) = find(inner, b"iinf") else {
        return Vec::new();
    };
    let count_bytes = if data[iinf.body] == 0 { 2 } else { 4 };
    let mut out = Vec::new();
    for infe in boxes(data, iinf.body + 4 + count_bytes, iinf.end)
        .iter()
        .filter(|b| &b.kind == b"infe")
    {
        let version = data[infe.body];
        if version < 2 {
            continue;
        }
        let mut at = infe.body + 4;
        let Some(id) = read(data, &mut at, if version == 2 { 2 } else { 4 }) else {
            continue;
        };
        at += 2;
        let Some(kind) = data.get(at..at + 4) else {
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
            out.push(id);
        }
    }
    out
}

/// The file ranges `iloc` gives `item`.
fn extents(data: &[u8], inner: &[Box4], item: u64) -> Vec<(usize, usize)> {
    let Some(iloc) = find(inner, b"iloc") else {
        return Vec::new();
    };
    let version = data[iloc.body];
    let parse = || -> Option<Vec<(usize, usize)>> {
        let mut at = iloc.body + 4;
        let sizes = read(data, &mut at, 1)?;
        let more = read(data, &mut at, 1)?;
        let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 15) as usize);
        let base_size = (more >> 4) as usize;
        let index_size = if version >= 1 {
            (more & 15) as usize
        } else {
            0
        };
        let wide = if version < 2 { 2 } else { 4 };
        let count = read(data, &mut at, wide)?;
        for _ in 0..count {
            let id = read(data, &mut at, wide)?;
            let method = if version >= 1 {
                read(data, &mut at, 2)? & 15
            } else {
                0
            };
            read(data, &mut at, 2)?;
            let base = read(data, &mut at, base_size)?;
            let n = read(data, &mut at, 2)?;
            let mut ranges = Vec::new();
            for _ in 0..n {
                read(data, &mut at, index_size)?;
                let off = read(data, &mut at, offset_size)?;
                let len = read(data, &mut at, length_size)?;
                ranges.push((off, len));
            }
            if id != item {
                continue;
            }
            // Offsets are into the file, or with method 1 into `idat`.
            let origin = match method {
                0 => 0,
                1 => find(inner, b"idat")?.body as u64,
                _ => return None,
            };
            return ranges
                .into_iter()
                .map(|(off, len)| {
                    let start = usize::try_from(origin + base + off).ok()?;
                    let end = if len == 0 {
                        data.len()
                    } else {
                        start.checked_add(usize::try_from(len).ok()?)?
                    };
                    (end <= data.len()).then_some((start, end))
                })
                .collect();
        }
        Some(Vec::new())
    };
    parse().unwrap_or_default()
}

/// One of the primary item's transformative properties, in the order they apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `irot`: this many quarter turns anticlockwise.
    Rotate(u8),
    /// `imir`: 0 mirrors about a vertical axis (left and right swap), 1 a horizontal one.
    Mirror(u8),
}

fn transforms(data: &[u8], inner: &[Box4], item: u64) -> Option<Vec<Op>> {
    let iprp = find(inner, b"iprp")?;
    let props = boxes(data, iprp.body, iprp.end);
    let ipco = boxes(
        data,
        find(&props, b"ipco")?.body,
        find(&props, b"ipco")?.end,
    );
    let ipma = find(&props, b"ipma")?;
    let version = data[ipma.body];
    let wide_index = data[ipma.body + 3] & 1 == 1;
    let mut at = ipma.body + 4;
    let entries = read(data, &mut at, 4)?;
    for _ in 0..entries {
        let id = read(data, &mut at, if version < 1 { 2 } else { 4 })?;
        let n = read(data, &mut at, 1)?;
        let mut indices = Vec::new();
        for _ in 0..n {
            let index = if wide_index {
                read(data, &mut at, 2)? & 0x7FFF
            } else {
                read(data, &mut at, 1)? & 0x7F
            };
            indices.push(index);
        }
        if id != item {
            continue;
        }
        return Some(
            indices
                .into_iter()
                .filter_map(|i| ipco.get(usize::try_from(i).ok()?.checked_sub(1)?))
                .filter_map(|p| match &p.kind {
                    b"irot" => Some(Op::Rotate(data.get(p.body)? & 3)),
                    b"imir" => Some(Op::Mirror(data.get(p.body)? & 1)),
                    _ => None,
                })
                .collect(),
        );
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

/// `data` with the EXIF and XMP items blanked, or `None` when it has none.
pub(super) fn strip(data: &[u8], m: &Meta) -> Option<Vec<u8>> {
    if m.metadata
        .iter()
        .all(|(s, e)| data[*s..*e].iter().all(|&b| b == 0))
    {
        return None;
    }
    let mut out = data.to_vec();
    for &(start, end) in &m.metadata {
        out[start..end].fill(0);
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
}
