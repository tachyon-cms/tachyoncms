//! Images made for tests: what a camera or an attacker would send, built in a few lines so
//! no binary fixture is checked in.

use std::io::Cursor;

use ::image::{DynamicImage, ImageFormat as Codec, RgbImage, Rgba, RgbaImage};
use img_parts::ImageEXIF;

/// `img` in `format`.
pub fn encoded(img: &DynamicImage, format: Codec) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// A `w`×`h` RGB gradient.
pub fn gradient(w: u32, h: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
        ::image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 128])
    }))
}

/// EXIF with `orientation`, and a GPS latitude when `gps`.
pub fn exif(orientation: Option<u16>, gps: bool) -> Vec<u8> {
    let mut entries: Vec<[u8; 12]> = Vec::new();
    let entry = |tag: u16, kind: u16, value: [u8; 4]| {
        let mut e = [0u8; 12];
        e[..2].copy_from_slice(&tag.to_be_bytes());
        e[2..4].copy_from_slice(&kind.to_be_bytes());
        e[4..8].copy_from_slice(&1u32.to_be_bytes());
        e[8..].copy_from_slice(&value);
        e
    };
    if let Some(o) = orientation {
        let v = o.to_be_bytes();
        entries.push(entry(0x0112, 3, [v[0], v[1], 0, 0]));
    }
    let ifd0_len = 2 + 12 * (entries.len() + usize::from(gps)) + 4;
    if gps {
        entries.push(entry(0x8825, 4, ((8 + ifd0_len) as u32).to_be_bytes()));
    }
    let mut out = b"MM\0\x2a\0\0\0\x08".to_vec();
    out.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for e in &entries {
        out.extend_from_slice(e);
    }
    out.extend_from_slice(&[0; 4]);
    if gps {
        out.extend_from_slice(&1u16.to_be_bytes());
        let mut e = entry(0x0001, 2, *b"N\0\0\0");
        e[4..8].copy_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&e);
        out.extend_from_slice(&[0; 4]);
    }
    out
}

pub fn with_exif(jpeg: Vec<u8>, exif: Vec<u8>) -> Vec<u8> {
    let mut j = img_parts::jpeg::Jpeg::from_bytes(jpeg.into()).unwrap();
    j.set_exif(Some(exif.into()));
    j.encoder().bytes().to_vec()
}

/// A PNG claiming `w`×`h`, with junk where its pixels would be.
pub fn png_header(w: u32, h: u32) -> Vec<u8> {
    let full = encoded(&gradient(1, 1), Codec::Png);
    let mut png = img_parts::png::Png::from_bytes(full.into()).unwrap();
    let ihdr = &mut png.chunks_mut()[0];
    let mut data = ihdr.contents().to_vec();
    data[..4].copy_from_slice(&w.to_be_bytes());
    data[4..8].copy_from_slice(&h.to_be_bytes());
    *ihdr = img_parts::png::PngChunk::new(*b"IHDR", data.into());
    let mut out = png.encoder().bytes().to_vec();
    out.truncate(out.len() - 12);
    out.extend_from_slice(&[0x55; 4000]);
    out
}

/// A GIF of `frames` copies of a `w`×`h` frame, with a comment.
pub fn gif(w: u32, h: u32, frames: usize) -> Vec<u8> {
    let one = encoded(
        &DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba([9, 9, 9, 255]))),
        Codec::Gif,
    );
    let packed = one[10];
    let start = 13
        + if packed & 0x80 != 0 {
            3 << ((packed & 7) + 1)
        } else {
            0
        };
    let body = &one[start..one.len() - 1];
    let mut out = one[..start].to_vec();
    out.extend_from_slice(b"\x21\xFE\x05hello\x00");
    for _ in 0..frames {
        out.extend_from_slice(body);
    }
    out.push(0x3B);
    out
}

/// A baseline CMYK JPEG, Adobe marker and all, every pixel the same.
pub fn cmyk_jpeg(w: u16, h: u16) -> Vec<u8> {
    // Four components, no subsampling, one 8×8 block each per MCU. Every block has only a
    // DC coefficient of 0 after the first, so the scan is the same code repeated: DC
    // difference category 0 (code `00`), then end-of-block (code `1010`).
    let mut out = vec![0xFF, 0xD8];
    let mut segment = |marker: u8, body: &[u8]| {
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
    };
    segment(0xEE, b"Adobe\0\x64\0\0\0\0\0");
    let mut dqt = vec![0u8];
    dqt.extend_from_slice(&[1; 64]);
    segment(0xDB, &dqt);
    let mut sof = vec![8];
    sof.extend_from_slice(&h.to_be_bytes());
    sof.extend_from_slice(&w.to_be_bytes());
    sof.push(4);
    for id in 1..=4u8 {
        sof.extend_from_slice(&[id, 0x11, 0]);
    }
    segment(0xC0, &sof);
    // DC table 0: one code of length 2 for category 0.
    let mut dc = vec![0x00, 0, 1];
    dc.extend_from_slice(&[0; 14]);
    dc.push(0);
    segment(0xC4, &dc);
    // AC table 0: codes `00`, `01` for 0x01, 0x02; `100` for 0x03; `1010` for EOB (0x00).
    let mut ac = vec![0x10, 0, 2, 1, 1];
    ac.extend_from_slice(&[0; 12]);
    ac.extend_from_slice(&[0x01, 0x02, 0x03, 0x00]);
    segment(0xC4, &ac);
    let mut sos = vec![4];
    for id in 1..=4u8 {
        sos.extend_from_slice(&[id, 0x00]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    segment(0xDA, &sos);
    let blocks = usize::from(w.div_ceil(8)) * usize::from(h.div_ceil(8)) * 4;
    // Each block is `00` `1010`: 6 bits. Four blocks make 3 bytes.
    let mut bits = String::new();
    for _ in 0..blocks {
        bits.push_str("001010");
    }
    while !bits.len().is_multiple_of(8) {
        bits.push('1');
    }
    for chunk in bits.as_bytes().chunks(8) {
        let byte = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 2).unwrap();
        out.push(byte);
        if byte == 0xFF {
            out.push(0);
        }
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// A raw deflate stream of `n` zero bytes, written as fixed-Huffman copies of 258.
fn deflate_zeros(n: usize) -> Vec<u8> {
    struct Bits {
        out: Vec<u8>,
        acc: u64,
        len: u32,
    }
    impl Bits {
        fn put(&mut self, value: u32, bits: u32) {
            self.acc |= u64::from(value) << self.len;
            self.len += bits;
            while self.len >= 8 {
                self.out.push(self.acc as u8);
                self.acc >>= 8;
                self.len -= 8;
            }
        }
        /// A Huffman code, which deflate stores most significant bit first.
        fn code(&mut self, code: u32, bits: u32) {
            let reversed = code.reverse_bits() >> (32 - bits);
            self.put(reversed, bits);
        }
    }
    let mut b = Bits {
        out: Vec::new(),
        acc: 0,
        len: 0,
    };
    b.put(1, 1); // the last block
    b.put(1, 2); // fixed Huffman codes
    b.code(0x30, 8); // a literal 0
    let mut left = n.saturating_sub(1);
    while left >= 258 {
        b.code(0b1100_0101, 8); // length 258
        b.code(0, 5); // distance 1
        left -= 258;
    }
    for _ in 0..left {
        b.code(0x30, 8);
    }
    b.code(0, 7); // end of block
    b.put(0, 7);
    b.out
}

/// `data` as a zlib stream.
fn zlib(n_zeros: usize) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(&deflate_zeros(n_zeros));
    let b = (n_zeros % 65521) as u32;
    out.extend_from_slice(&((b << 16) | 1).to_be_bytes());
    out
}

/// A 1×1 PNG whose ICC profile inflates to `inflated` zero bytes.
pub fn iccp_bomb_png(inflated: usize) -> Vec<u8> {
    let full = encoded(&rgba(1, 1), Codec::Png);
    let mut png = img_parts::png::Png::from_bytes(full.into()).unwrap();
    let mut body = b"bomb\0\0".to_vec();
    body.extend_from_slice(&zlib(inflated));
    png.chunks_mut()
        .insert(1, img_parts::png::PngChunk::new(*b"iCCP", body.into()));
    png.encoder().bytes().to_vec()
}

/// A `w`×`h` opaque RGBA image, the kind an icon holds.
pub fn rgba(w: u32, h: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba([1, 2, 3, 255])))
}

/// An ICO holding `png` as its one entry.
pub fn ico_around(png: &[u8]) -> Vec<u8> {
    let side = |at: usize| png[at + 3]; // IHDR's width and height, under 256 here
    let mut out = vec![0, 0, 1, 0, 1, 0];
    out.extend_from_slice(&[side(16), side(20), 0, 0, 1, 0, 32, 0]);
    out.extend_from_slice(&(png.len() as u32).to_le_bytes());
    out.extend_from_slice(&22u32.to_le_bytes());
    out.extend_from_slice(png);
    out
}

/// The boxes directly inside `data`: (type, start of the box, start of its body, end).
fn boxes(data: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = from;
    while i + 8 <= to {
        let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[i + 4..i + 8].try_into().unwrap();
        let end = if size == 0 { to } else { i + size };
        out.push((kind, i, i + 8, end));
        i = end;
    }
    out
}

/// Where the iloc of AVIF `data` puts its one item's single extent: the offset of the
/// extent's (offset, length) fields, which are 4 bytes each, and the values.
fn first_extent(data: &[u8]) -> (usize, usize, usize) {
    let meta = boxes(data, 0, data.len())
        .into_iter()
        .find(|b| &b.0 == b"meta")
        .unwrap();
    let iloc = boxes(data, meta.2 + 4, meta.3)
        .into_iter()
        .find(|b| &b.0 == b"iloc")
        .unwrap();
    let body = iloc.2;
    let version = data[body];
    let sizes = data[body + 4];
    let more = data[body + 5];
    assert_eq!(
        (sizes, more >> 4),
        (0x44, 0),
        "4-byte offsets and lengths, no base"
    );
    let mut i = body + 6;
    i += if version < 2 { 2 } else { 4 }; // item count
    i += if version < 2 { 2 } else { 4 }; // item ID
    if version >= 1 {
        i += 2;
    }
    i += 2; // data reference index
    i += 2; // extent count
    let at = |j: usize| u32::from_be_bytes(data[j..j + 4].try_into().unwrap()) as usize;
    (i, at(i), at(i + 4))
}

/// The sequence header OBU at the front of an AV1 item.
fn sequence_header(item: &[u8]) -> Vec<u8> {
    let mut i = 0;
    loop {
        let header = item[i];
        let mut j = i + 1 + usize::from(header & 4 != 0);
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let b = item[j];
            j += 1;
            size |= usize::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        if (header >> 3) & 0xF == 1 {
            return item[i..j + size].to_vec();
        }
        i = j + size;
    }
}

/// A `w`×`h` AVIF with the sequence header of a 1×1 one put in front of its own, so its
/// first header claims far fewer pixels than it decodes to.
pub fn avif_two_headers(w: u32, h: u32) -> Vec<u8> {
    let small = encoded(&gradient(1, 1), Codec::Avif);
    let (_, off, len) = first_extent(&small);
    let lie = sequence_header(&small[off..off + len]);
    let mut big = encoded(&gradient(w, h), Codec::Avif);
    let (field, off, len) = first_extent(&big);
    big[field + 4..field + 8].copy_from_slice(&((len + lie.len()) as u32).to_be_bytes());
    let mdat = boxes(&big, 0, big.len())
        .into_iter()
        .find(|b| &b.0 == b"mdat")
        .unwrap();
    let size = (mdat.3 - mdat.1 + lie.len()) as u32;
    big[mdat.1..mdat.1 + 4].copy_from_slice(&size.to_be_bytes());
    big.splice(off..off, lie);
    big
}

/// Grows the size field of the box starting at `at` by `by`.
fn grow(data: &mut [u8], at: usize, by: usize) {
    let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    data[at..at + 4].copy_from_slice(&((size + by) as u32).to_be_bytes());
}

/// A `w`×`h` AVIF whose primary item carries an `irot` of `quarter_turns` anticlockwise.
pub fn avif_rotated(w: u32, h: u32, quarter_turns: u8) -> Vec<u8> {
    avif_with_property(
        w,
        h,
        &[0, 0, 0, 9, b'i', b'r', b'o', b't', quarter_turns & 3],
    )
}

/// A `w`×`h` AVIF with a clean aperture of `cw`×`ch`, centred.
pub fn avif_cropped(w: u32, h: u32, cw: u32, ch: u32) -> Vec<u8> {
    let mut clap = vec![0, 0, 0, 40];
    clap.extend_from_slice(b"clap");
    for v in [cw, 1, ch, 1, 0, 1, 0, 1] {
        clap.extend_from_slice(&v.to_be_bytes());
    }
    avif_with_property(w, h, &clap)
}

/// A `w`×`h` AVIF whose primary item carries the property box `property` as well.
pub fn avif_with_property(w: u32, h: u32, property: &[u8]) -> Vec<u8> {
    let mut data = encoded(&gradient(w, h), Codec::Avif);
    let meta = boxes(&data, 0, data.len())
        .into_iter()
        .find(|b| &b.0 == b"meta")
        .unwrap();
    let inner = boxes(&data, meta.2 + 4, meta.3);
    let iprp = inner.iter().find(|b| &b.0 == b"iprp").unwrap();
    let props = boxes(&data, iprp.2, iprp.3);
    let ipco = props.iter().find(|b| &b.0 == b"ipco").unwrap();
    let ipma = props.iter().find(|b| &b.0 == b"ipma").unwrap();
    let count = boxes(&data, ipco.2, ipco.3).len();
    assert_eq!(data[ipma.2 + 3] & 1, 0, "one-byte property indices");
    assert!(data[ipma.2] == 0, "two-byte item IDs");
    // The first entry is the primary item: its association count, then its indices.
    let entries = u32::from_be_bytes(data[ipma.2 + 4..ipma.2 + 8].try_into().unwrap());
    assert!(entries >= 1);
    let n_at = ipma.2 + 8 + 2;
    let n = usize::from(data[n_at]);
    let (ipco_end, ipma_insert) = (ipco.3, n_at + 1 + n);
    let (meta_at, iprp_at, ipco_at, ipma_at) = (meta.1, iprp.1, ipco.1, ipma.1);
    // The property after the existing ones, and its index after the item's others.
    let by = property.len() + 1;
    data.insert(ipma_insert, 0x80 | (count as u8 + 1));
    data[n_at] += 1;
    grow(&mut data, ipma_at, 1);
    data.splice(ipco_end..ipco_end, property.iter().copied());
    grow(&mut data, ipco_at, property.len());
    grow(&mut data, iprp_at, by);
    grow(&mut data, meta_at, by);
    // The picture moved on: so do its extents.
    let (field, off, _) = first_extent(&data);
    data[field..field + 4].copy_from_slice(&((off + by) as u32).to_be_bytes());
    data
}

/// A `w`×`h` AVIF carrying `exif` as an EXIF item.
pub fn avif_with_exif(w: u32, h: u32, exif: Vec<u8>) -> Vec<u8> {
    use ::image::ImageEncoder;
    let mut out = Vec::new();
    let mut e = ::image::codecs::avif::AvifEncoder::new(&mut out);
    e.set_exif_metadata(exif).unwrap();
    gradient(w, h).write_with_encoder(e).unwrap();
    out
}

/// An uncompressed 2×2 grey TIFF with an ImageDescription of `description`.
pub fn tiff_with_description(description: &str) -> Vec<u8> {
    let mut text = description.as_bytes().to_vec();
    text.push(0);
    let entries: [(u16, u16, u32, u32); 9] = [
        (256, 3, 1, 2),                 // width
        (257, 3, 1, 2),                 // height
        (258, 3, 1, 8),                 // bits a sample
        (259, 3, 1, 1),                 // no compression
        (262, 3, 1, 1),                 // black is zero
        (270, 2, text.len() as u32, 0), // the description, placed below
        (273, 4, 1, 0),                 // strip offset, placed below
        (278, 3, 1, 2),                 // rows a strip
        (279, 4, 1, 4),                 // strip bytes
    ];
    let ifd_len = 2 + 12 * entries.len() + 4;
    let text_at = 8 + ifd_len;
    let pixels_at = text_at + text.len();
    let mut out = b"II*\0\x08\0\0\0".to_vec();
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in entries {
        let value = match tag {
            270 => text_at as u32,
            273 => pixels_at as u32,
            _ => value,
        };
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&text);
    out.extend_from_slice(&[10, 80, 160, 240]);
    out
}

/// A `w`×`h` PNG with an ICC profile of `icc_len` zero bytes and EXIF orientation 6.
pub fn png_icc_turned(w: u32, h: u32, icc_len: usize) -> Vec<u8> {
    let full = encoded(&gradient(w, h), Codec::Png);
    let mut png = img_parts::png::Png::from_bytes(full.into()).unwrap();
    let mut body = b"Display P3\0\0".to_vec();
    body.extend_from_slice(&zlib(icc_len));
    png.chunks_mut()
        .insert(1, img_parts::png::PngChunk::new(*b"iCCP", body.into()));
    png.set_exif(Some(exif(Some(6), false).into()));
    png.encoder().bytes().to_vec()
}

/// Every extent `iloc` gives, in order: the item id, where its (offset, length) fields are,
/// and their values. Like `first_extent`, for 4-byte fields and no base offset.
fn all_extents(data: &[u8]) -> Vec<(u32, usize, usize, usize)> {
    let meta = boxes(data, 0, data.len())
        .into_iter()
        .find(|b| &b.0 == b"meta")
        .unwrap();
    let iloc = boxes(data, meta.2 + 4, meta.3)
        .into_iter()
        .find(|b| &b.0 == b"iloc")
        .unwrap();
    let body = iloc.2;
    let version = data[body];
    assert_eq!((data[body + 4], data[body + 5] >> 4), (0x44, 0));
    let wide = if version < 2 { 2 } else { 4 };
    let num = |j: usize, n: usize| {
        data[j..j + n]
            .iter()
            .fold(0usize, |v, &b| v << 8 | b as usize)
    };
    let mut i = body + 6;
    let count = num(i, wide);
    i += wide;
    let mut out = Vec::new();
    for _ in 0..count {
        let id = num(i, wide) as u32;
        i += wide;
        if version >= 1 {
            i += 2;
        }
        i += 2;
        let n = num(i, 2);
        i += 2;
        for _ in 0..n {
            out.push((id, i, num(i, 4), num(i + 4, 4)));
            i += 8;
        }
    }
    out
}

/// Every iloc extent of `data`: (item id, offset, length), with the fields' position
/// second.
pub fn avif_extents(data: &[u8]) -> Vec<(u32, usize, usize)> {
    all_extents(data)
        .into_iter()
        .map(|(id, _, off, len)| (id, off, len))
        .collect()
}

/// `data` with the `index`th extent rewritten to `offset` and `length`.
pub fn avif_set_extent(mut data: Vec<u8>, index: usize, offset: u32, length: u32) -> Vec<u8> {
    let (_, at, _, _) = all_extents(&data)[index];
    data[at..at + 4].copy_from_slice(&offset.to_be_bytes());
    data[at + 4..at + 8].copy_from_slice(&length.to_be_bytes());
    data
}

/// The OBUs of an AV1 item: (type, start, end).
fn obus(item: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < item.len() {
        let header = item[i];
        let mut j = i + 1 + usize::from(header & 4 != 0);
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let b = item[j];
            j += 1;
            size |= usize::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        out.push(((header >> 3) & 0xF, i, j + size));
        i = j + size;
    }
    out
}

/// A `w`×`h` AVIF whose item holds its frame OBU twice.
pub fn avif_two_frames(w: u32, h: u32) -> Vec<u8> {
    let mut data = encoded(&gradient(w, h), Codec::Avif);
    let (field, off, len) = first_extent(&data);
    let item = data[off..off + len].to_vec();
    let (_, start, end) = obus(&item)
        .into_iter()
        .find(|o| o.0 == 6 || o.0 == 3)
        .expect("a frame OBU");
    let frame = item[start..end].to_vec();
    data[field + 4..field + 8].copy_from_slice(&((len + frame.len()) as u32).to_be_bytes());
    let mdat = boxes(&data, 0, data.len())
        .into_iter()
        .find(|b| &b.0 == b"mdat")
        .unwrap();
    grow(&mut data, mdat.1, frame.len());
    data.splice(off + len..off + len, frame);
    data
}

/// A GIF of `frames` different `w`×`h` frames.
pub fn moving_gif(w: u32, h: u32, frames: usize) -> Vec<u8> {
    use ::image::codecs::gif::{GifEncoder, Repeat};
    use ::image::{Delay, Frame};
    let mut out = Vec::new();
    {
        let mut e = GifEncoder::new(&mut out);
        e.set_repeat(Repeat::Infinite).unwrap();
        for f in 0..frames {
            let v = (f * 255 / frames.max(1)) as u8;
            let img = RgbaImage::from_pixel(w, h, Rgba([v, 255 - v, 0, 255]));
            e.encode_frame(Frame::from_parts(
                img,
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
        }
    }
    out
}

/// An ICO with `entries` directory entries, all pointing at `png`.
pub fn ico_repeating(png: &[u8], entries: u16) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&entries.to_le_bytes());
    let at = 6 + 16 * u32::from(entries);
    for _ in 0..entries {
        out.extend_from_slice(&[png[19], png[23], 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
    }
    out.extend_from_slice(png);
    out
}

/// An ICO of `pngs`, each its own entry.
pub fn ico_of(pngs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(pngs.len() as u16).to_le_bytes());
    let mut at = 6 + 16 * pngs.len();
    for png in pngs {
        out.extend_from_slice(&[png[19], png[23], 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(at as u32).to_le_bytes());
        at += png.len();
    }
    for png in pngs {
        out.extend_from_slice(png);
    }
    out
}
