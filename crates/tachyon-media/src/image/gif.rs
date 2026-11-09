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
}

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
    loop {
        let start = i;
        let Some(kind) = at(i) else { break };
        match kind {
            0x21 => {
                let (Some(label), Some(end)) = (at(i + 1), sub_blocks(i + 2)) else {
                    break;
                };
                let looping = bytes
                    .get(i + 3..i + 14)
                    .is_some_and(|id| id == b"NETSCAPE2.0" || id == b"ANIMEXTS1.0");
                if label != 0xFE && (label != 0xFF || looping) {
                    keep(&mut out, start, end);
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
    })
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
