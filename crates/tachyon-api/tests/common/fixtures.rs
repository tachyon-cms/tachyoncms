//! Hostile files made on the spot, so the repository stays small: headers claiming far more
//! pixels than the file holds, animations whose frames add up past the cap, and files that
//! are two formats at once. The committed fixtures beside them are in `fixtures/uploads`.

use image::{DynamicImage, ImageFormat, RgbImage};
use tachyon_media::fixtures::{encoded, gradient};

/// The committed fixture `name`, from `tests/fixtures/uploads`.
pub fn committed(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/uploads/");
    std::fs::read(format!("{path}{name}")).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// A 2×2 image in `format`.
fn tiny(format: ImageFormat) -> Vec<u8> {
    encoded(&gradient(2, 2), format)
}

/// Header bombs: each a small, otherwise well-formed file whose header claims at least
/// 100 million pixels, with the format's name.
pub fn header_bombs() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("png", tachyon_media::fixtures::png_header(100_000, 100_000)),
        ("jpeg", jpeg_claiming(65_000, 65_000)),
        ("gif", gif_claiming(65_535, 65_535)),
        ("webp", webp_claiming(10_000, 10_000)),
        ("bmp", bmp_claiming(30_000, 30_000)),
    ]
}

/// A JPEG whose start-of-frame says `w`×`h`.
pub fn jpeg_claiming(w: u16, h: u16) -> Vec<u8> {
    let mut out = tiny(ImageFormat::Jpeg);
    let sof = (2..out.len() - 1)
        .find(|&i| out[i] == 0xFF && matches!(out[i + 1], 0xC0..=0xC2))
        .expect("a start of frame");
    out[sof + 5..sof + 7].copy_from_slice(&h.to_be_bytes());
    out[sof + 7..sof + 9].copy_from_slice(&w.to_be_bytes());
    out
}

/// A one-frame GIF whose screen and frame both say `w`×`h`.
pub fn gif_claiming(w: u16, h: u16) -> Vec<u8> {
    let mut out = tiny(ImageFormat::Gif);
    out[6..8].copy_from_slice(&w.to_le_bytes());
    out[8..10].copy_from_slice(&h.to_le_bytes());
    let descriptor = out.iter().position(|&b| b == 0x2C).expect("a frame");
    out[descriptor + 5..descriptor + 7].copy_from_slice(&w.to_le_bytes());
    out[descriptor + 7..descriptor + 9].copy_from_slice(&h.to_le_bytes());
    out
}

/// A lossless WebP whose bitstream header says `w`×`h` (each under 16,384).
pub fn webp_claiming(w: u32, h: u32) -> Vec<u8> {
    let mut out = tiny(ImageFormat::WebP);
    assert_eq!(&out[12..16], b"VP8L", "the encoder writes lossless");
    // After the 0x2F signature: 14 bits of width - 1, then 14 of height - 1.
    let mut bits = u32::from_le_bytes(out[21..25].try_into().unwrap());
    bits &= !((1 << 28) - 1);
    bits |= (w - 1) | ((h - 1) << 14);
    out[21..25].copy_from_slice(&bits.to_le_bytes());
    out
}

/// A BMP whose info header says `w`×`h`.
pub fn bmp_claiming(w: i32, h: i32) -> Vec<u8> {
    let mut out = tiny(ImageFormat::Bmp);
    out[18..22].copy_from_slice(&w.to_le_bytes());
    out[22..26].copy_from_slice(&h.to_le_bytes());
    out
}

/// Frame bombs: animations whose frames, each within the cap, add up to 51 million pixels,
/// just over the 50 million cap, with the format's name.
pub fn frame_bombs() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("gif", tachyon_media::fixtures::gif(1000, 1000, 51)),
        ("webp", animated_webp(1000, 1000, 51)),
    ]
}

/// An animated WebP: a `w`×`h` canvas and `frames` frames, each a lossless 1×1 at the top
/// left. A decoder draws every frame onto the whole canvas.
pub fn animated_webp(w: u32, h: u32, frames: usize) -> Vec<u8> {
    let one = encoded(
        &DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([200, 30, 30]))),
        ImageFormat::WebP,
    );
    let bitstream = &one[12..];
    let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
    let chunk = |kind: &[u8], body: &[u8]| {
        let mut c = kind.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    };
    // Animation flag; canvas width - 1 and height - 1.
    let mut vp8x = vec![0x02, 0, 0, 0];
    vp8x.extend(u24(w - 1));
    vp8x.extend(u24(h - 1));
    let mut body = b"WEBP".to_vec();
    body.extend(chunk(b"VP8X", &vp8x));
    // A background colour, and loop forever.
    body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
    for _ in 0..frames {
        let mut frame = Vec::new();
        frame.extend(u24(0));
        frame.extend(u24(0));
        frame.extend(u24(0));
        frame.extend(u24(0));
        frame.extend(u24(100));
        frame.push(0);
        frame.extend_from_slice(bitstream);
        body.extend(chunk(b"ANMF", &frame));
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// What a polyglot carries after its PNG, so a test can look for it.
pub const POLYGLOT_TAIL: &str = "<script>alert(document.domain)</script>";

/// A valid `w`×`h` PNG followed by an HTML page.
pub fn png_html(w: u32, h: u32) -> Vec<u8> {
    let mut out = super::png(w, h);
    out.extend_from_slice(format!("<html><body>{POLYGLOT_TAIL}</body></html>").as_bytes());
    out
}

/// A valid `w`×`h` PNG followed by a ZIP holding `index.html`: a PNG to an image reader, an
/// archive to a ZIP reader (which reads from the end).
pub fn png_zip(w: u32, h: u32) -> Vec<u8> {
    let mut out = super::png(w, h);
    let name = b"index.html";
    let data = POLYGLOT_TAIL.as_bytes();
    let crc = crc32(data);
    let local_at = out.len() as u32;
    // Local file header, stored (no compression).
    out.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    // Central directory.
    let central_at = out.len() as u32;
    out.extend_from_slice(b"PK\x01\x02\x14\x00\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&local_at.to_le_bytes());
    out.extend_from_slice(name);
    let central_len = out.len() as u32 - central_at;
    // End of central directory.
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00");
    out.extend_from_slice(&central_len.to_le_bytes());
    out.extend_from_slice(&central_at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in bytes {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}
