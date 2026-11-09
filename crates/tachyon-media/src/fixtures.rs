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
