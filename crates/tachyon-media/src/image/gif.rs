//! A GIF's blocks, walked without decoding, and its frames decoded onto a canvas with a
//! memory limit the decoder is held to.

use std::io::Cursor;

use ::image::{Rgba, RgbaImage};

use super::ImageError;

/// What walking a GIF's blocks finds.
pub(super) struct Walked {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    /// The pixels the frames make the decoder hold: each the larger of the canvas and the
    /// frame itself.
    pub pixels: u64,
    /// The file without comments and application extensions other than the looping ones
    /// (XMP travels in those). Empty unless asked for.
    pub stripped: Vec<u8>,
    /// Whether the trailer was reached; a file that stops after a whole frame is still
    /// read, as browsers read it.
    pub ends: bool,
    /// An ICC profile (`ICCRGBG1012`) and XMP packet (`XMP DataXMP`) the file carries.
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
}

const ICC_ID: &[u8] = b"ICCRGBG1012";
const XMP_ID: &[u8] = b"XMP DataXMP";

/// Walks `bytes`. A file that stops before its first whole frame is `Truncated`.
pub(super) fn walk(bytes: &[u8], strip: bool) -> Result<Walked, ImageError> {
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) || bytes.len() < 13 {
        return Err(ImageError::Truncated);
    }
    let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    let screen = u64::from(width) * u64::from(height);
    let mut i = 13;
    if bytes[10] & 0x80 != 0 {
        i += 3 << ((bytes[10] & 7) + 1);
    }
    let mut out = Vec::new();
    let keep = |out: &mut Vec<u8>, from: usize, to: usize| {
        if strip {
            out.extend_from_slice(&bytes[from..to.min(bytes.len())]);
        }
    };
    keep(&mut out, 0, i);
    let at = |i: usize| bytes.get(i).copied();
    // The end of the sub-blocks starting at `i`.
    let sub_blocks = |mut i: usize| -> Option<usize> {
        loop {
            let n = usize::from(at(i)?);
            i += 1 + n;
            if n == 0 {
                return (i <= bytes.len()).then_some(i);
            }
        }
    };
    let (mut frames, mut pixels, mut ends) = (0u32, 0u64, false);
    let (mut icc, mut xmp) = (None, None);
    // The graphic control block for the next image: only the last one counts, and it's
    // written just before that image, so repeating them carries nothing.
    let mut control: Option<[u8; 8]> = None;
    loop {
        let start = i;
        let Some(kind) = at(i) else { break };
        match kind {
            0x21 => {
                let (Some(label), Some(end)) = (at(i + 1), sub_blocks(i + 2)) else {
                    break;
                };
                let id = bytes
                    .get(i + 3..i + 14)
                    .filter(|_| label == 0xFF && at(i + 2) == Some(11));
                let data = || sub_block_data(bytes, i + 14, end);
                // What's kept is rebuilt, so no payload rides along unchecked: graphic
                // control blocks of their fixed shape (reserved bits cleared), the loop
                // count, and a profile that looks like one. Comments, plain text and
                // anything else go.
                let rebuilt: Option<Vec<u8>> = match (label, id) {
                    (0xF9, _) => {
                        if let Some(b) = bytes.get(start..end).filter(|b| b.len() == 8 && b[2] == 4)
                        {
                            control = Some([0x21, 0xF9, 4, b[3] & 0x1F, b[4], b[5], b[6], 0]);
                        }
                        None
                    }
                    (0xFF, Some(id)) if id == b"NETSCAPE2.0" || id == b"ANIMEXTS1.0" => {
                        let d = data();
                        (d.len() == 3 && d[0] == 1).then(|| {
                            let mut b = vec![0x21, 0xFF, 11];
                            b.extend_from_slice(id);
                            b.extend_from_slice(&[3, 1, d[1], d[2], 0]);
                            b
                        })
                    }
                    (0xFF, Some(id)) if id == ICC_ID => {
                        let d = data();
                        let plausible = d.get(36..40) == Some(b"acsp")
                            && d.get(..4).is_some_and(|n| {
                                u32::from_be_bytes([n[0], n[1], n[2], n[3]]) as usize == d.len()
                            });
                        plausible.then(|| {
                            let b = extension(ICC_ID, &d);
                            icc = Some(d);
                            b
                        })
                    }
                    (0xFF, Some(id)) if id == XMP_ID => {
                        // Written raw, ended by a 257-byte "magic trailer" and a zero so it
                        // walks as sub-blocks; taken only with that exact trailer.
                        let packet = end
                            .checked_sub(258)
                            .filter(|&t| t >= i + 14 && bytes.get(t..end) == Some(&trailer()[..]));
                        xmp = packet
                            .and_then(|t| bytes.get(i + 14..t))
                            .map(<[u8]>::to_vec);
                        None
                    }
                    _ => None,
                };
                if let (true, Some(b)) = (strip, rebuilt) {
                    out.extend_from_slice(&b);
                }
                i = end;
            }
            0x2C => {
                let Some(d) = bytes.get(i + 1..i + 10) else {
                    break;
                };
                let w = u64::from(u16::from_le_bytes([d[4], d[5]]));
                let h = u64::from(u16::from_le_bytes([d[6], d[7]]));
                let mut j = i + 10;
                if d[8] & 0x80 != 0 {
                    j += 3 << ((d[8] & 7) + 1);
                }
                let Some(end) = sub_blocks(j + 1) else { break };
                i = end;
                frames += 1;
                pixels = pixels.saturating_add(screen.max(w * h));
                if let (true, Some(c)) = (strip, control.take()) {
                    out.extend_from_slice(&c);
                }
                keep(&mut out, start, i);
            }
            0x3B => {
                keep(&mut out, start, i + 1);
                ends = true;
                break;
            }
            b => {
                return Err(ImageError::Corrupt(format!(
                    "a GIF block starts with {b:#04x}"
                )));
            }
        }
    }
    if frames == 0 {
        return Err(ImageError::Truncated);
    }
    if strip && !ends {
        out.push(0x3B);
    }
    Ok(Walked {
        width,
        height,
        frames,
        pixels,
        stripped: out,
        ends,
        icc,
        xmp,
    })
}

/// The XMP "magic trailer": 1, 255 down to 0, then the block terminator.
fn trailer() -> Vec<u8> {
    let mut t = vec![1];
    t.extend((0..=255u8).rev());
    t.push(0);
    t
}

/// An application extension `id` carrying `data` as sub-blocks.
fn extension(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x21, 0xFF, 11];
    out.extend_from_slice(id);
    for block in data.chunks(255) {
        out.push(block.len() as u8);
        out.extend_from_slice(block);
    }
    out.push(0);
    out
}

/// Whether `data` walks as sub-blocks ending exactly at its last byte.
fn walks_as_sub_blocks(data: &[u8]) -> bool {
    let mut at = 0;
    while let Some(&n) = data.get(at) {
        at += 1 + usize::from(n);
        if n == 0 {
            return at == data.len();
        }
    }
    false
}

/// The data of the sub-blocks from `at` to `end`, joined.
fn sub_block_data(bytes: &[u8], mut at: usize, end: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while at < end {
        let n = usize::from(bytes[at]);
        if n == 0 {
            break;
        }
        out.extend_from_slice(bytes.get(at + 1..at + 1 + n).unwrap_or_default());
        at += 1 + n;
    }
    out
}

/// The GIF `bytes` (as an encoder made them) with `icc` and `xmp` as application
/// extensions after the screen descriptor.
pub(super) fn with_meta(bytes: &[u8], icc: Option<&[u8]>, xmp: Option<&[u8]>) -> Vec<u8> {
    let Some(&packed) = bytes.get(10) else {
        return bytes.to_vec();
    };
    let mut at = 13;
    if packed & 0x80 != 0 {
        at += 3 << ((packed & 7) + 1);
    }
    let at = at.min(bytes.len());
    let mut out = bytes[..at].to_vec();
    if let Some(icc) = icc {
        out.extend(extension(ICC_ID, icc));
    }
    // Raw XMP steers the sub-block walk; it's written only if, trailer and all, it walks
    // to its own end and can't open blocks of its own.
    let packet = xmp.map(|x| [x, &trailer()[..]].concat());
    if let Some(packet) = packet.filter(|p| walks_as_sub_blocks(p)) {
        out.extend_from_slice(&[0x21, 0xFF, 11]);
        out.extend_from_slice(XMP_ID);
        out.extend_from_slice(&packet);
    }
    out.extend_from_slice(&bytes[at..]);
    out
}

/// A frame as it's shown, with how long, in milliseconds.
pub(super) struct Shown {
    pub image: RgbaImage,
    pub delay_ms: u32,
}

/// The frames of `bytes` as they're shown (each drawn over the last, as its disposal
/// says), the first only unless `all`, with each frame's memory held to `limit` bytes.
/// Also the loop count, `None` for forever.
pub(super) fn frames(
    bytes: &[u8],
    all: bool,
    limit: u64,
) -> Result<(Vec<Shown>, Option<u16>), ImageError> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let limit = std::num::NonZeroU64::new(limit.max(1)).expect("at least 1");
    options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    let mut d = options.read_info(Cursor::new(bytes)).map_err(error)?;
    let (w, h) = (u32::from(d.width()), u32::from(d.height()));
    let loops = match d.repeat() {
        gif::Repeat::Infinite => None,
        gif::Repeat::Finite(n) => Some(n),
    };
    let mut canvas = RgbaImage::new(w, h);
    let mut out = Vec::new();
    loop {
        let frame = match d.read_next_frame() {
            Ok(Some(f)) => f,
            Ok(None) => break,
            // A file cut off after a whole frame shows what it has.
            Err(_) if !out.is_empty() => break,
            Err(e) => return Err(error(e)),
        };
        let previous = (frame.dispose == gif::DisposalMethod::Previous).then(|| canvas.clone());
        let (left, top) = (u32::from(frame.left), u32::from(frame.top));
        let (fw, fh) = (u32::from(frame.width), u32::from(frame.height));
        for y in 0..fh {
            for x in 0..fw {
                let at = 4 * (y * fw + x) as usize;
                let Some(p) = frame.buffer.get(at..at + 4) else {
                    continue;
                };
                if p[3] != 0 && left + x < w && top + y < h {
                    canvas.put_pixel(left + x, top + y, Rgba([p[0], p[1], p[2], p[3]]));
                }
            }
        }
        out.push(Shown {
            image: canvas.clone(),
            delay_ms: u32::from(frame.delay) * 10,
        });
        if !all {
            break;
        }
        match (frame.dispose, previous) {
            (gif::DisposalMethod::Background, _) => {
                for y in top..(top + fh).min(h) {
                    for x in left..(left + fw).min(w) {
                        canvas.put_pixel(x, y, Rgba([0, 0, 0, 0]));
                    }
                }
            }
            (_, Some(p)) => canvas = p,
            _ => {}
        }
    }
    Ok((out, loops))
}

fn error(e: gif::DecodingError) -> ImageError {
    match e {
        gif::DecodingError::UnexpectedEof => ImageError::Truncated,
        gif::DecodingError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            ImageError::Truncated
        }
        gif::DecodingError::MemoryLimit | gif::DecodingError::OutOfMemory => ImageError::OverMemory,
        e => ImageError::Corrupt(e.to_string()),
    }
}
