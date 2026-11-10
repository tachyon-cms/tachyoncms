//! An icon's directory and the chunks of its PNG entries, walked within fixed bounds; its
//! entries decoded from exactly their declared bytes; and the icon rebuilt with those
//! PNGs' metadata chunks gone and every entry kept.
//!
//! image's own icon decoder isn't used: it reads a PNG entry from its offset to the end of
//! the file with no limits, so what follows an entry's declared size reaches it.

use super::{ImageError, PNG_KEEP};

/// The most entries an icon may list: real ones hold a handful of sizes.
const MAX_ENTRIES: usize = 256;
/// The most PNG chunks walked over all of an icon's entries.
const MAX_CHUNKS: usize = 1 << 16;

/// A PNG chunk: its type and where it starts and ends (length to CRC, inclusive).
pub(super) type Chunk = ([u8; 4], usize, usize);

/// The whole chunks of PNG `data`, up to a cut-off one if there is one, counting each
/// against `budget` and refusing past it.
pub(super) fn png_chunks(data: &[u8], budget: &mut usize) -> Result<Vec<Chunk>, ImageError> {
    let mut out = Vec::new();
    let mut i = 8;
    while i < data.len() {
        *budget = budget
            .checked_sub(1)
            .ok_or_else(|| ImageError::Corrupt("this image has too many PNG chunks".into()))?;
        let Some(h) = data.get(i..i + 8) else { break };
        let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let Some(end) = i.checked_add(12).and_then(|e| e.checked_add(len)) else {
            break;
        };
        if end > data.len() {
            break;
        }
        let kind = [h[4], h[5], h[6], h[7]];
        out.push((kind, i, end));
        if &kind == b"IEND" {
            break;
        }
        i = end;
    }
    Ok(out)
}

/// One directory entry: its 16 bytes, and the range of its image.
pub(super) struct Entry {
    dir: [u8; 16],
    pub start: usize,
    pub end: usize,
    /// For a PNG entry, its chunks.
    chunks: Option<Vec<Chunk>>,
    /// The size its own header gives (a PNG's IHDR, a bitmap's info header).
    pub width: u32,
    pub height: u32,
}

impl Entry {
    pub fn is_png(&self) -> bool {
        self.chunks.is_some()
    }
}

/// An icon, walked.
pub(super) struct Icon {
    pub entries: Vec<Entry>,
    /// Whether any PNG entry carries a chunk outside `PNG_KEEP`.
    pub metadata: bool,
}

/// Walks the icon `bytes`. Each distinct image is walked once however many entries point
/// at it. Refuses more than `MAX_ENTRIES` entries, more than `MAX_CHUNKS` chunks in all,
/// entries leaving the file or overlapping without being the same, and any PNG entry with
/// compressed metadata (`iCCP`, `zTXt`, `iTXt`).
pub(super) fn walk(bytes: &[u8]) -> Result<Icon, ImageError> {
    let cut = || ImageError::Corrupt("the icon's directory is cut off".into());
    let count = usize::from(u16::from_le_bytes(
        bytes
            .get(4..6)
            .ok_or_else(cut)?
            .try_into()
            .map_err(|_| cut())?,
    ));
    if count > MAX_ENTRIES {
        return Err(ImageError::Corrupt(format!(
            "this icon lists {count} images; at most {MAX_ENTRIES} are taken"
        )));
    }
    let mut budget = MAX_CHUNKS;
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    let mut metadata = false;
    for e in 0..count {
        let dir: [u8; 16] = bytes
            .get(6 + 16 * e..22 + 16 * e)
            .ok_or_else(cut)?
            .try_into()
            .map_err(|_| cut())?;
        let size = u32::from_le_bytes([dir[8], dir[9], dir[10], dir[11]]) as usize;
        let start = u32::from_le_bytes([dir[12], dir[13], dir[14], dir[15]]) as usize;
        let end = start.checked_add(size).ok_or_else(cut)?;
        let data = bytes.get(start..end).ok_or_else(cut)?;
        if let Some(seen) = entries.iter().find(|x| (x.start, x.end) == (start, end)) {
            let (chunks, width, height) = (seen.chunks.clone(), seen.width, seen.height);
            entries.push(Entry {
                dir,
                start,
                end,
                chunks,
                width,
                height,
            });
            continue;
        }
        // Distinct entries can't share bytes: copying each would multiply the file.
        if entries.iter().any(|x| x.start < end && start < x.end) {
            return Err(ImageError::Corrupt("this icon's images overlap".into()));
        }
        let chunks = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            let chunks = png_chunks(data, &mut budget)?;
            for (kind, _, _) in &chunks {
                if matches!(kind, b"iCCP" | b"zTXt" | b"iTXt") {
                    return Err(ImageError::Corrupt(
                        "an icon's PNG carries compressed metadata".into(),
                    ));
                }
                metadata |= !PNG_KEEP.contains(&kind);
            }
            Some(chunks)
        } else {
            None
        };
        let le32 = |at: usize| {
            data.get(at..at + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let be32 = |at: usize| {
            data.get(at..at + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        };
        let (width, height) = if chunks.is_some() {
            (be32(16).unwrap_or(0), be32(20).unwrap_or(0))
        } else {
            // A bitmap's height counts its mask too.
            let h = le32(8).map_or(0, |h| (h as i32).unsigned_abs() / 2);
            (le32(4).map_or(0, |w| (w as i32).unsigned_abs()), h)
        };
        entries.push(Entry {
            dir,
            start,
            end,
            chunks,
            width,
            height,
        });
    }
    Ok(Icon { entries, metadata })
}

/// The icon `bytes` rebuilt with its PNG entries' chunks outside `PNG_KEEP` gone, every
/// entry kept, or `None` when there's nothing to take out.
pub(super) fn strip(bytes: &[u8], icon: &Icon) -> Option<Vec<u8>> {
    if !icon.metadata {
        return None;
    }
    let header = bytes.get(..6)?;
    let mut images: Vec<((usize, usize), Vec<u8>)> = Vec::new();
    for e in &icon.entries {
        if images.iter().any(|(r, _)| *r == (e.start, e.end)) {
            continue;
        }
        let data = bytes.get(e.start..e.end)?;
        let image = match &e.chunks {
            Some(chunks) => {
                let mut out = data.get(..8)?.to_vec();
                for (kind, s, end) in chunks {
                    if PNG_KEEP.contains(&kind) {
                        out.extend_from_slice(data.get(*s..*end)?);
                    }
                }
                out
            }
            None => data.to_vec(),
        };
        images.push(((e.start, e.end), image));
    }
    let mut out = header.to_vec();
    let mut at = 6 + 16 * icon.entries.len();
    let mut placed: Vec<((usize, usize), usize)> = Vec::new();
    for (range, image) in &images {
        placed.push((*range, at));
        at += image.len();
    }
    for e in &icon.entries {
        let (i, &(_, offset)) = placed
            .iter()
            .enumerate()
            .find(|(_, (r, _))| *r == (e.start, e.end))?;
        let mut dir = e.dir;
        dir[8..12].copy_from_slice(&u32::try_from(images[i].1.len()).ok()?.to_le_bytes());
        dir[12..16].copy_from_slice(&u32::try_from(offset).ok()?.to_le_bytes());
        out.extend_from_slice(&dir);
    }
    for (_, image) in &images {
        out.extend_from_slice(image);
    }
    Some(out)
}

impl Icon {
    /// The entry shown as the icon: the largest.
    pub fn largest(&self) -> Option<&Entry> {
        self.entries
            .iter()
            .max_by_key(|e| u64::from(e.width) * u64::from(e.height))
    }
}

/// The bitmap (DIB) entry `data` as RGBA: 1, 4, 8, 24 or 32 bits a pixel, uncompressed
/// (or 32-bit bit fields), with its AND mask for transparency. Reads only `data`, and
/// refuses one over `cap` pixels before allocating.
pub(super) fn decode_bitmap(data: &[u8], cap: u64) -> Result<::image::RgbaImage, ImageError> {
    let cut = || ImageError::Corrupt("an icon's bitmap is cut off".into());
    let u32_at = |i: usize| -> Result<u32, ImageError> {
        let b = data.get(i..i + 4).ok_or_else(cut)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let header = u32_at(0)? as usize;
    let (w, h) = (u32_at(4)? as i32, u32_at(8)? as i32);
    // An icon's bitmap is bottom-up: a top-down (negative) height isn't one.
    if w <= 0 || h <= 0 || header < 40 {
        return Err(ImageError::Corrupt("an icon's bitmap has no size".into()));
    }
    let (w, h) = (w.unsigned_abs(), h.unsigned_abs() / 2);
    let bits = u16::from_le_bytes(
        data.get(14..16)
            .ok_or_else(cut)?
            .try_into()
            .map_err(|_| cut())?,
    );
    let compression = u32_at(16)?;
    if h == 0 {
        return Err(ImageError::Corrupt("an icon's bitmap has no size".into()));
    }
    if u64::from(w) * u64::from(h) > cap {
        return Err(ImageError::TooManyPixels {
            pixels: u64::from(w) * u64::from(h),
            cap,
        });
    }
    // Bit fields: the three channel masks sit at 40 (after a 40-byte header, inside a
    // longer one). Only the one layout a plain 32-bit bitmap has is taken.
    let bit_fields = compression == 3 && bits == 32;
    if bit_fields && [u32_at(40)?, u32_at(44)?, u32_at(48)?] != [0x00FF_0000, 0x0000_FF00, 0xFF] {
        return Err(ImageError::Unsupported(
            "this icon bitmap's channel layout isn't supported",
        ));
    }
    if !(compression == 0 || bit_fields) {
        return Err(ImageError::Unsupported(
            "compressed icon bitmaps aren't supported",
        ));
    }
    let header = if bit_fields { header.max(52) } else { header };
    let colours = match bits {
        1 | 4 | 8 => {
            let used = u32_at(32)? as usize;
            if used == 0 {
                1 << bits
            } else {
                used.min(1 << bits)
            }
        }
        24 | 32 => 0,
        _ => {
            return Err(ImageError::Unsupported(
                "this icon bitmap's depth isn't supported",
            ));
        }
    };
    let palette = data.get(header..header + 4 * colours).ok_or_else(cut)?;
    let stride = (w as usize * usize::from(bits)).div_ceil(32) * 4;
    let pixels_at = header + 4 * colours;
    let mask_stride = (w as usize).div_ceil(32) * 4;
    let mask_at = pixels_at + stride * h as usize;
    let mut out = ::image::RgbaImage::new(w, h);
    // Rows run bottom to top. The AND mask clears a pixel; a 32-bit bitmap carries its own
    // alpha instead, unless that's zero throughout (older icons), when the mask is used.
    let masked = |x: usize, y: usize| {
        data.get(mask_at + mask_stride * y..mask_at + mask_stride * (y + 1))
            .is_some_and(|m| (m[x / 8] >> (7 - x % 8)) & 1 == 1)
    };
    for y in 0..h as usize {
        let row = data
            .get(pixels_at + stride * y..pixels_at + stride * (y + 1))
            .ok_or_else(cut)?;
        for x in 0..w as usize {
            let index = |b: u16| -> usize {
                let bit = x * usize::from(b);
                let shift = 8 - usize::from(b) - bit % 8;
                usize::from((u16::from(row[bit / 8]) >> shift) & ((1u16 << b) - 1))
            };
            let [r, g, b, a] = match bits {
                24 => [row[3 * x + 2], row[3 * x + 1], row[3 * x], 255],
                32 => [row[4 * x + 2], row[4 * x + 1], row[4 * x], row[4 * x + 3]],
                n => {
                    let i = index(n);
                    let p = palette.get(4 * i..4 * i + 4).ok_or_else(cut)?;
                    [p[2], p[1], p[0], 255]
                }
            };
            let a = if bits == 32 || !masked(x, y) { a } else { 0 };
            out.put_pixel(
                x as u32,
                (h as usize - 1 - y) as u32,
                ::image::Rgba([r, g, b, a]),
            );
        }
    }
    if bits == 32 && out.pixels().all(|p| p[3] == 0) {
        for (x, y, p) in out.enumerate_pixels_mut() {
            p[3] = if masked(x as usize, h as usize - 1 - y as usize) {
                0
            } else {
                255
            };
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use ::image::DynamicImage;
    use ::image::codecs::bmp::BmpDecoder;

    use super::*;

    /// A 4×2 DIB of `bits` a pixel: an info header of `header` bytes with `compression`,
    /// `palette` entries, `rows` (top first, each already packed and padded), and for an
    /// icon (`mask` given) a doubled height, an AND mask after the pixels and anything in
    /// `after_header` between the header and the palette.
    #[allow(clippy::too_many_arguments)]
    fn dib(
        bits: u16,
        header: u32,
        compression: u32,
        after_header: &[u8],
        palette: &[[u8; 4]],
        rows: &[&[u8]],
        mask: Option<&[u8]>,
    ) -> Vec<u8> {
        let h = rows.len() as u32;
        let mut out = Vec::new();
        for v in [header, 4, if mask.is_some() { 2 * h } else { h }] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&compression.to_le_bytes());
        out.extend_from_slice(&[0; 20]);
        out.extend_from_slice(after_header);
        out.extend(palette.iter().flatten());
        for row in rows.iter().rev() {
            out.extend_from_slice(row);
        }
        if let Some(m) = mask {
            out.extend_from_slice(m);
        }
        out
    }

    fn icon(dib: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 1, 0, 1, 0, 4, 2, 0, 0, 1, 0, 0, 0];
        out.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        out.extend_from_slice(&22u32.to_le_bytes());
        out.extend_from_slice(dib);
        out
    }

    /// `dib` (no mask) as a BMP file, decoded by image's own decoder.
    fn oracle(dib: &[u8], colours: usize) -> ::image::RgbImage {
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&((54 + 4 * colours) as u32).to_le_bytes());
        bmp.extend_from_slice(dib);
        let d = BmpDecoder::new(Cursor::new(bmp)).unwrap();
        DynamicImage::from_decoder(d).unwrap().to_rgb8()
    }

    fn palette(n: usize) -> Vec<[u8; 4]> {
        (0..n)
            .map(|i| [i as u8, (i * 3) as u8, (i * 7) as u8, 0])
            .collect()
    }

    /// `indices`, `bits` each, packed most significant first and padded to 4 bytes.
    fn packed(bits: u16, indices: &[u8]) -> Vec<u8> {
        let mut row = vec![0u8; 4];
        for (x, &i) in indices.iter().enumerate() {
            let bit = x * usize::from(bits);
            row[bit / 8] |= i << (8 - usize::from(bits) - bit % 8);
        }
        row
    }

    #[test]
    fn palette_bitmaps_match_images_decoder() {
        for (bits, top, bottom) in [
            (1, [0u8, 1, 1, 0], [1u8, 0, 0, 1]),
            (4, [0, 1, 15, 8], [7, 2, 9, 14]),
            (8, [0, 1, 128, 255], [200, 17, 64, 3]),
        ] {
            let colours = 1 << bits;
            let (top, bottom) = (packed(bits, &top), packed(bits, &bottom));
            let plain = dib(bits, 40, 0, &[], &palette(colours), &[&top, &bottom], None);
            let ico = icon(&dib(
                bits,
                40,
                0,
                &[],
                &palette(colours),
                &[&top, &bottom],
                Some(&[0; 8]),
            ));
            let ours = DynamicImage::ImageRgba8(decode_bitmap(&ico[22..], 100).unwrap());
            assert_eq!(ours.to_rgb8(), oracle(&plain, colours), "{bits}-bit");
            assert!(ours.to_rgba8().pixels().all(|p| p[3] == 255), "{bits}-bit");
        }
    }

    #[test]
    fn true_colour_bitmaps_match_images_decoder() {
        let top24: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let bottom24: &[u8] = &[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24];
        let top32: &[u8] = &[1, 2, 3, 200, 4, 5, 6, 0, 7, 8, 9, 100, 10, 11, 12, 255];
        let bottom32: &[u8] = &[13, 14, 15, 1, 16, 17, 18, 2, 19, 20, 21, 3, 22, 23, 24, 4];
        for (bits, rows) in [(24u16, [top24, bottom24]), (32, [top32, bottom32])] {
            let plain = dib(bits, 40, 0, &[], &[], &rows, None);
            let ico = icon(&dib(bits, 40, 0, &[], &[], &rows, Some(&[0; 8])));
            let ours = DynamicImage::ImageRgba8(decode_bitmap(&ico[22..], 100).unwrap());
            assert_eq!(ours.to_rgb8(), oracle(&plain, 0), "{bits}-bit");
        }
        // 32-bit alpha is kept when any pixel has some.
        let ico = icon(&dib(32, 40, 0, &[], &[], &[top32, bottom32], Some(&[0; 8])));
        let img = decode_bitmap(&ico[22..], 100).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1, 200]);
        assert_eq!(img.get_pixel(1, 0).0, [6, 5, 4, 0]);
        assert_eq!(img.get_pixel(3, 1).0, [24, 23, 22, 4]);
    }

    #[test]
    fn all_zero_alpha_falls_back_to_the_and_mask() {
        let top: &[u8] = &[1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        let bottom: &[u8] = &[13, 14, 15, 0, 16, 17, 18, 0, 19, 20, 21, 0, 22, 23, 24, 0];
        // Mask rows bottom to top: the bottom row's first pixel and the top row's last.
        let mask: &[u8] = &[0x80, 0, 0, 0, 0x10, 0, 0, 0];
        let ico = icon(&dib(32, 40, 0, &[], &[], &[top, bottom], Some(mask)));
        let img = decode_bitmap(&ico[22..], 100).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1, 255]);
        assert_eq!(img.get_pixel(3, 0).0, [12, 11, 10, 0]);
        assert_eq!(img.get_pixel(0, 1).0, [15, 14, 13, 0]);
        assert_eq!(img.get_pixel(1, 1).0, [18, 17, 16, 255]);
    }

    #[test]
    fn a_top_down_bitmap_is_refused() {
        let row: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut ico = icon(&dib(24, 40, 0, &[], &[], &[row, row], Some(&[0; 8])));
        ico[22 + 8..22 + 12].copy_from_slice(&(-4i32).to_le_bytes());
        assert!(matches!(
            decode_bitmap(&ico[22..], 100),
            Err(ImageError::Corrupt(_))
        ));
    }

    #[test]
    fn bit_fields_after_a_40_byte_header_start_the_pixels_at_52() {
        let top: &[u8] = &[1, 2, 3, 200, 4, 5, 6, 0, 7, 8, 9, 100, 10, 11, 12, 255];
        let bottom: &[u8] = &[13, 14, 15, 1, 16, 17, 18, 2, 19, 20, 21, 3, 22, 23, 24, 4];
        let masks = |m: [u32; 3]| m.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let bgr = masks([0x00FF_0000, 0x0000_FF00, 0x0000_00FF]);
        let ico = icon(&dib(32, 40, 3, &bgr, &[], &[top, bottom], Some(&[0; 8])));
        let img = decode_bitmap(&ico[22..], 100).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1, 200]);
        assert_eq!(img.get_pixel(3, 1).0, [24, 23, 22, 4]);
        // Masks in a 108-byte header sit inside it; the pixels follow the header.
        let mut v4 = bgr.clone();
        v4.extend_from_slice(&[0; 56]);
        let ico = icon(&dib(32, 108, 3, &v4, &[], &[top, bottom], Some(&[0; 8])));
        assert_eq!(
            decode_bitmap(&ico[22..], 100).unwrap().get_pixel(0, 0).0,
            [3, 2, 1, 200]
        );
        // Any other layout isn't taken.
        let rgb = masks([0x0000_00FF, 0x0000_FF00, 0x00FF_0000]);
        let ico = icon(&dib(32, 40, 3, &rgb, &[], &[top, bottom], Some(&[0; 8])));
        assert!(matches!(
            decode_bitmap(&ico[22..], 100),
            Err(ImageError::Unsupported(_))
        ));
    }
}
