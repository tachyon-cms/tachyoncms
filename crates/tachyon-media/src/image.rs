//! What's inside an uploaded image: its size, read from the headers before anything is
//! decoded, and the original as it's stored, with its metadata gone, turned upright, and
//! trimmed, resized and re-encoded as the collection says.
//!
//! Every pixel buffer is made after `probe` has said how big it will be, and the decoders
//! are held to the same cap, so a small file claiming a huge image is refused unread.

use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};

use ::image::codecs::gif::GifEncoder;
use ::image::codecs::jpeg::JpegEncoder;
use ::image::{
    DynamicImage, Frame, ImageDecoder, ImageFormat as Codec, ImageReader, Limits, Rgba, RgbaImage,
    metadata::Orientation,
};
use fast_image_resize::{ResizeOptions as Fir, Resizer};
use img_parts::{Bytes, ImageEXIF};
use tachyon_core::uploads::{
    EncodeOptions, FailOn, Fit, ImageFormat, Position, TrimOptions, Upload,
};

/// The formats an original can be in: what decodes. The formats made are `ImageFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
    Gif,
    Tiff,
    Bmp,
    Ico,
}

/// An image's size from its headers alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    /// The EXIF orientation, 1 to 8; 1 when there's none.
    pub orientation: u8,
    pub format: InputFormat,
}

/// The original as it's to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    /// The input itself when it's stored as uploaded, else a new file in `out_dir`.
    pub path: PathBuf,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub filesize: u64,
    pub blurhash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("this image is {pixels} pixels, all its frames counted; the limit is {cap}")]
    TooManyPixels { pixels: u64, cap: u64 },
    #[error("this image is cut off before its end")]
    Truncated,
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("this image can't be read: {0}")]
    Corrupt(String),
    /// The server's fault: a file it couldn't write.
    #[error("{0}")]
    Internal(String),
}

/// The types `probe` reads, HEIC among them only to refuse it by name.
pub fn decodes(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg"
            | "image/png"
            | "image/webp"
            | "image/avif"
            | "image/gif"
            | "image/tiff"
            | "image/bmp"
            | "image/vnd.microsoft.icon"
            | "image/x-icon"
            | "image/heif"
            | "image/heic"
    )
}

/// The most pixels an original of `u` may decode to.
pub fn cap(u: &Upload, max_pixels: u64) -> u64 {
    u.constructor_options
        .as_ref()
        .and_then(|c| c.limit_input_pixels)
        .map_or(max_pixels, |l| l.min(max_pixels))
}

/// `TooManyPixels` when `p`, all frames counted, is over `cap`.
pub fn check_cap(p: &Probe, cap: u64) -> Result<(), ImageError> {
    let pixels = u64::from(p.width) * u64::from(p.height) * u64::from(p.frames);
    if pixels > cap {
        return Err(ImageError::TooManyPixels { pixels, cap });
    }
    Ok(())
}

fn corrupt(e: impl std::fmt::Display) -> ImageError {
    ImageError::Corrupt(e.to_string())
}

fn internal(e: std::io::Error) -> ImageError {
    ImageError::Internal(format!("can't process an image: {e}"))
}

fn from_image(e: ::image::ImageError) -> ImageError {
    match e {
        ::image::ImageError::IoError(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            ImageError::Truncated
        }
        ::image::ImageError::Unsupported(e) => ImageError::Corrupt(e.to_string()),
        e => {
            let text = e.to_string();
            let lower = text.to_ascii_lowercase();
            if lower.contains("end of") || lower.contains("eof") || lower.contains("truncat") {
                ImageError::Truncated
            } else {
                ImageError::Corrupt(text)
            }
        }
    }
}

/// What the file starting with `head` at `path` is, how big and how many frames, from its
/// headers: no pixel buffer is made.
pub fn probe(path: &Path, head: &[u8]) -> Result<Probe, ImageError> {
    let format = match ::image::guess_format(head) {
        Ok(Codec::Jpeg) => InputFormat::Jpeg,
        Ok(Codec::Png) => InputFormat::Png,
        Ok(Codec::WebP) => InputFormat::Webp,
        Ok(Codec::Avif) => InputFormat::Avif,
        Ok(Codec::Gif) => InputFormat::Gif,
        Ok(Codec::Tiff) => InputFormat::Tiff,
        Ok(Codec::Bmp) => InputFormat::Bmp,
        Ok(Codec::Ico) => InputFormat::Ico,
        _ if head.get(4..8) == Some(b"ftyp") => {
            return Err(ImageError::Unsupported("HEIC isn't supported yet"));
        }
        _ => return Err(ImageError::Unsupported("this image format isn't supported")),
    };
    let (width, height, frames, orientation) = match format {
        InputFormat::Avif => {
            let bytes = std::fs::read(path).map_err(internal)?;
            let avif = avif_parse::read_avif(&mut bytes.as_slice()).map_err(avif_error)?;
            // The frame size the decoder will allocate, not the size the container claims.
            let color = avif.primary_item_metadata().map_err(avif_error)?;
            let alpha = avif.alpha_item_metadata().map_err(avif_error)?;
            let w = alpha.as_ref().map_or(0, |a| a.max_frame_width.get());
            let h = alpha.as_ref().map_or(0, |a| a.max_frame_height.get());
            (
                color.max_frame_width.get().max(w),
                color.max_frame_height.get().max(h),
                1,
                1,
            )
        }
        _ => {
            let (w, h, o) = headers(path, format)?;
            let frames = match format {
                InputFormat::Gif => gif_blocks(&std::fs::read(path).map_err(internal)?, false)?.0,
                InputFormat::Webp => webp_frames(&std::fs::read(path).map_err(internal)?),
                _ => 1,
            };
            (w, h, frames, o)
        }
    };
    Ok(Probe {
        width,
        height,
        frames,
        orientation,
        format,
    })
}

fn avif_error(e: avif_parse::Error) -> ImageError {
    match e {
        avif_parse::Error::UnexpectedEOF => ImageError::Truncated,
        e => corrupt(e),
    }
}

fn codec(f: InputFormat) -> Codec {
    match f {
        InputFormat::Jpeg => Codec::Jpeg,
        InputFormat::Png => Codec::Png,
        InputFormat::Webp => Codec::WebP,
        InputFormat::Avif => Codec::Avif,
        InputFormat::Gif => Codec::Gif,
        InputFormat::Tiff => Codec::Tiff,
        InputFormat::Bmp => Codec::Bmp,
        InputFormat::Ico => Codec::Ico,
    }
}

/// Width, height and EXIF orientation, from a decoder that has read only the headers.
fn headers(path: &Path, format: InputFormat) -> Result<(u32, u32, u8), ImageError> {
    let file = BufReader::new(File::open(path).map_err(internal)?);
    let mut reader = ImageReader::with_format(file, codec(format));
    // Checked against the cap by the caller, which knows it.
    reader.no_limits();
    let mut decoder = reader.into_decoder().map_err(from_image)?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().map_or(1, |o| o.to_exif()).clamp(1, 8);
    Ok((w, h, orientation))
}

/// The frames of the GIF `bytes`, and, when `strip`, the file without its comments and
/// application extensions other than the looping ones (XMP travels in those).
fn gif_blocks(bytes: &[u8], strip: bool) -> Result<(u32, Vec<u8>), ImageError> {
    let at = |i: usize| bytes.get(i).copied().ok_or(ImageError::Truncated);
    let mut out = Vec::new();
    let mut i = 13;
    let packed = at(10)?;
    if packed & 0x80 != 0 {
        i += 3 << ((packed & 7) + 1);
    }
    let keep = |out: &mut Vec<u8>, from: usize, to: usize| {
        if strip {
            out.extend_from_slice(&bytes[from..to]);
        }
    };
    keep(&mut out, 0, i.min(bytes.len()));
    // The end of the sub-blocks starting at `i`.
    let sub_blocks = |mut i: usize| -> Result<usize, ImageError> {
        loop {
            let n = usize::from(at(i)?);
            i += 1 + n;
            if n == 0 {
                return Ok(i);
            }
        }
    };
    let mut frames = 0u32;
    loop {
        let start = i;
        match at(i)? {
            0x21 => {
                let label = at(i + 1)?;
                let end = sub_blocks(i + 2)?;
                let looping = bytes
                    .get(i + 3..i + 14)
                    .is_some_and(|id| id == b"NETSCAPE2.0" || id == b"ANIMEXTS1.0");
                if label != 0xFE && (label != 0xFF || looping) {
                    keep(&mut out, start, end);
                }
                i = end;
            }
            0x2C => {
                let packed = at(i + 9)?;
                i += 10;
                if packed & 0x80 != 0 {
                    i += 3 << ((packed & 7) + 1);
                }
                i = sub_blocks(i + 1)?;
                frames += 1;
                keep(&mut out, start, i);
            }
            0x3B => {
                keep(&mut out, start, i + 1);
                break;
            }
            b => {
                return Err(ImageError::Corrupt(format!(
                    "a GIF block starts with {b:#04x}"
                )));
            }
        }
    }
    Ok((frames.max(1), out))
}

/// The frames of the WebP `bytes`: its `ANMF` chunks, or 1.
fn webp_frames(bytes: &[u8]) -> u32 {
    let mut frames = 0u32;
    let mut i = 12;
    while let Some(h) = bytes.get(i..i + 8) {
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        if &h[..4] == b"ANMF" {
            frames = frames.saturating_add(1);
        }
        i = i
            .saturating_add(8)
            .saturating_add(len)
            .saturating_add(len & 1);
    }
    frames.max(1)
}

/// Whether the JPEG `bytes` ends with its end-of-image marker, trailing padding aside.
fn has_eoi(bytes: &[u8]) -> bool {
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    bytes[..end].ends_with(&[0xFF, 0xD9])
}

/// The first frame of `bytes`, upright, decoded with at most `cap` pixels.
fn decode(bytes: &[u8], p: &Probe, cap: u64) -> Result<DynamicImage, ImageError> {
    let mut img = if p.format == InputFormat::Avif {
        avif(bytes)?
    } else {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), codec(p.format));
        let side = u32::try_from(cap).unwrap_or(u32::MAX);
        let mut limits = Limits::default();
        limits.max_image_width = Some(side);
        limits.max_image_height = Some(side);
        // Eight bytes a pixel: 16-bit RGBA, the widest a decoder here makes.
        limits.max_alloc = Some(cap.saturating_mul(8));
        reader.limits(limits);
        let decoder = reader.into_decoder().map_err(from_image)?;
        DynamicImage::from_decoder(decoder).map_err(|e| match e {
            ::image::ImageError::Limits(_) => ImageError::TooManyPixels {
                pixels: u64::from(p.width) * u64::from(p.height) * u64::from(p.frames),
                cap,
            },
            e => from_image(e),
        })?
    };
    if let Some(o) = Orientation::from_exif(p.orientation) {
        img.apply_orientation(o);
    }
    Ok(img)
}

fn avif(bytes: &[u8]) -> Result<DynamicImage, ImageError> {
    use avif_decode::Image;
    let image = avif_decode::Decoder::from_avif(bytes)
        .and_then(|d| d.to_image())
        .map_err(|e| match e {
            avif_decode::Error::Avif(e) => avif_error(e),
            e => corrupt(e),
        })?;
    let bad = || ImageError::Corrupt("the AVIF's planes don't match its size".into());
    Ok(match image {
        Image::Rgb8(i) => {
            let (w, h) = (i.width() as u32, i.height() as u32);
            let px = i.pixels().flat_map(|p| [p.r, p.g, p.b]).collect();
            DynamicImage::ImageRgb8(::image::RgbImage::from_raw(w, h, px).ok_or_else(bad)?)
        }
        Image::Rgba8(i) => {
            let (w, h) = (i.width() as u32, i.height() as u32);
            let px = i.pixels().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
            DynamicImage::ImageRgba8(RgbaImage::from_raw(w, h, px).ok_or_else(bad)?)
        }
        Image::Rgb16(i) => {
            let (w, h) = (i.width() as u32, i.height() as u32);
            let px = i.pixels().flat_map(|p| [p.r, p.g, p.b]).collect();
            DynamicImage::ImageRgb16(::image::ImageBuffer::from_raw(w, h, px).ok_or_else(bad)?)
        }
        Image::Rgba16(i) => {
            let (w, h) = (i.width() as u32, i.height() as u32);
            let px = i.pixels().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
            DynamicImage::ImageRgba16(::image::ImageBuffer::from_raw(w, h, px).ok_or_else(bad)?)
        }
        Image::Gray8(i) => {
            let (w, h) = (i.width() as u32, i.height() as u32);
            let px = i.pixels().map(|p| p.value()).collect();
            DynamicImage::ImageLuma8(::image::GrayImage::from_raw(w, h, px).ok_or_else(bad)?)
        }
        Image::Gray16(i) => {
            let (w, h) = (i.width() as u32, i.height() as u32);
            let px = i.pixels().map(|p| p.value()).collect();
            DynamicImage::ImageLuma16(::image::ImageBuffer::from_raw(w, h, px).ok_or_else(bad)?)
        }
    })
}

/// The original `input`, as `probe` read it, made into what `u` stores: metadata stripped
/// unless `withMetadata` (call 9: turned upright first when its orientation isn't 1), then
/// `trimOptions`, `resizeOptions` and `formatOptions` in that order. Animated originals
/// keep their frames (call 8), so they're stored as uploaded, metadata aside.
pub fn process_original(
    input: &Path,
    probe: &Probe,
    u: &Upload,
    max_pixels: u64,
    out_dir: &Path,
) -> Result<Processed, ImageError> {
    let cap = cap(u, max_pixels);
    check_cap(probe, cap)?;
    let strict = u
        .constructor_options
        .as_ref()
        .is_none_or(|c| c.fail_on != FailOn::None);
    let bytes = std::fs::read(input).map_err(internal)?;
    if strict && probe.format == InputFormat::Jpeg && !has_eoi(&bytes) {
        return Err(ImageError::Truncated);
    }
    let img = match decode(&bytes, probe, cap) {
        Ok(img) => Some(img),
        Err(e @ ImageError::TooManyPixels { .. }) => return Err(e),
        Err(e) if strict => return Err(e),
        // `failOn: none`: stored without what a decode would give.
        Err(_) => None,
    };
    let animated = probe.frames > 1;
    let transforms =
        u.trim_options.is_some() || u.resize_options.is_some() || u.format_options.is_some();
    let encode = !animated
        && (transforms
            || (!u.with_metadata
                && (probe.orientation != 1
                    || matches!(probe.format, InputFormat::Tiff | InputFormat::Avif))));
    if let (true, Some(mut img)) = (encode, img.clone()) {
        if let Some(t) = &u.trim_options {
            img = trim(img, t);
        }
        if let Some(r) = &u.resize_options {
            img = resize(
                &img,
                r.width,
                r.height,
                r.fit,
                r.position,
                r.without_enlargement.unwrap_or(false),
            )?;
        }
        let (out, options) = match &u.format_options {
            Some(f) => (made(f.format), f.options.clone()),
            None => (probe.format, EncodeOptions::default()),
        };
        let path = write_new(out_dir, |f| encode_to(&img, out, &options, f))?;
        return processed(path, mime_of(out), &img);
    }
    if encode
        && !matches!(
            probe.format,
            InputFormat::Jpeg | InputFormat::Png | InputFormat::Webp
        )
    {
        // A TIFF or AVIF only loses its metadata by being re-encoded.
        return Err(ImageError::Corrupt(
            "its metadata can't be removed without decoding it".into(),
        ));
    }
    let stripped = if u.with_metadata {
        None
    } else {
        strip(probe.format, &bytes)?
    };
    let path = match stripped {
        Some(b) => write_new(out_dir, |f| f.write_all(&b).map_err(internal))?,
        None => input.to_path_buf(),
    };
    let filesize = std::fs::metadata(&path).map_err(internal)?.len();
    let upright = |w, h| {
        if probe.orientation >= 5 {
            (h, w)
        } else {
            (w, h)
        }
    };
    let (width, height) = img
        .as_ref()
        .map_or(upright(probe.width, probe.height), |i| {
            (i.width(), i.height())
        });
    Ok(Processed {
        path,
        mime: mime_of(probe.format),
        width,
        height,
        filesize,
        blurhash: img.as_ref().and_then(blurhash),
    })
}

fn processed(
    path: PathBuf,
    mime: &'static str,
    img: &DynamicImage,
) -> Result<Processed, ImageError> {
    Ok(Processed {
        filesize: std::fs::metadata(&path).map_err(internal)?.len(),
        path,
        mime,
        width: img.width(),
        height: img.height(),
        blurhash: blurhash(img),
    })
}

/// A new file in `dir` holding what `write` puts in it; removed again if that fails.
fn write_new(
    dir: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<(), ImageError>,
) -> Result<PathBuf, ImageError> {
    let file = tempfile::NamedTempFile::new_in(dir).map_err(internal)?;
    let mut out = BufWriter::new(file.as_file().try_clone().map_err(internal)?);
    write(&mut out)?;
    out.flush().map_err(internal)?;
    let (_, path) = file.keep().map_err(|e| internal(e.error))?;
    Ok(path)
}

/// `bytes` without EXIF, XMP, IPTC and comments, or `None` when there were none to take
/// out. Colour profiles stay.
fn strip(format: InputFormat, bytes: &[u8]) -> Result<Option<Vec<u8>>, ImageError> {
    let b = Bytes::copy_from_slice(bytes);
    let out = match format {
        InputFormat::Jpeg => {
            let mut jpeg = img_parts::jpeg::Jpeg::from_bytes(b).map_err(corrupt)?;
            let keep = |s: &img_parts::jpeg::JpegSegment| match s.marker() {
                // JFIF, an ICC profile, Adobe's colour transform: what decoding needs.
                0xE0 => s.contents().starts_with(b"JFIF\0"),
                0xE2 => s.contents().starts_with(b"ICC_PROFILE\0"),
                0xEE => s.contents().starts_with(b"Adobe"),
                0xE1..=0xEF | 0xFE => false,
                _ => true,
            };
            let segments = jpeg.segments_mut();
            segments.retain(keep);
            // The scan holds the rest of the file: whatever follows its end-of-image marker
            // (more pictures, with their own EXIF) goes too.
            let scan = segments
                .last()
                .filter(|s| s.has_entropy())
                .map(|s| s.len_with_entropy() - s.len());
            let mut out = jpeg.encoder().bytes();
            if let Some(scan) = scan {
                let start = out.len() - scan;
                if let Some(end) = out[start..].windows(2).position(|w| w == [0xFF, 0xD9]) {
                    out.truncate(start + end + 2);
                }
            }
            out
        }
        InputFormat::Png => {
            let mut png = img_parts::png::Png::from_bytes(b).map_err(corrupt)?;
            png.chunks_mut().retain(|c| {
                matches!(
                    &c.kind(),
                    b"IHDR"
                        | b"PLTE"
                        | b"IDAT"
                        | b"IEND"
                        | b"tRNS"
                        | b"cHRM"
                        | b"gAMA"
                        | b"iCCP"
                        | b"sBIT"
                        | b"sRGB"
                        | b"cICP"
                        | b"mDCv"
                        | b"cLLi"
                        | b"bKGD"
                        | b"hIST"
                        | b"pHYs"
                        | b"sPLT"
                        | b"acTL"
                        | b"fcTL"
                        | b"fdAT"
                )
            });
            png.encoder().bytes()
        }
        InputFormat::Webp => {
            let mut webp = img_parts::webp::WebP::from_bytes(b).map_err(corrupt)?;
            webp.chunks_mut().retain(|c| {
                matches!(
                    &c.id(),
                    b"VP8 " | b"VP8L" | b"VP8X" | b"ALPH" | b"ANIM" | b"ANMF" | b"ICCP"
                )
            });
            // Rewrites the VP8X flags to the chunks that are left.
            webp.set_exif(None);
            webp.encoder().bytes()
        }
        InputFormat::Gif => Bytes::from(gif_blocks(bytes, true)?.1),
        _ => return Ok(None),
    };
    Ok((out.as_ref() != bytes).then(|| out.to_vec()))
}

/// The input format standing for output format `f`.
fn made(f: ImageFormat) -> InputFormat {
    match f {
        ImageFormat::Jpeg => InputFormat::Jpeg,
        ImageFormat::Png => InputFormat::Png,
        ImageFormat::Webp => InputFormat::Webp,
        ImageFormat::Avif => InputFormat::Avif,
        ImageFormat::Gif => InputFormat::Gif,
    }
}

/// The type a file of `f` is stored under.
pub fn mime_of(f: InputFormat) -> &'static str {
    match f {
        InputFormat::Jpeg => "image/jpeg",
        InputFormat::Png => "image/png",
        InputFormat::Webp => "image/webp",
        InputFormat::Avif => "image/avif",
        InputFormat::Gif => "image/gif",
        InputFormat::Tiff => "image/tiff",
        InputFormat::Bmp => "image/bmp",
        InputFormat::Ico => "image/vnd.microsoft.icon",
    }
}

/// Encodes `img` as `out` into `f`, with `o`'s quality, losslessness and effort where the
/// format takes them. Defaults follow the usual ones: JPEG and WebP 80, AVIF 50 at effort 4.
fn encode_to(
    img: &DynamicImage,
    format: InputFormat,
    o: &EncodeOptions,
    f: &mut BufWriter<File>,
) -> Result<(), ImageError> {
    let failed =
        |e: ::image::ImageError| ImageError::Internal(format!("can't encode an image: {e}"));
    let opaque = || {
        if img.color().has_color() {
            DynamicImage::ImageRgb8(img.to_rgb8())
        } else {
            DynamicImage::ImageLuma8(img.to_luma8())
        }
    };
    match format {
        InputFormat::Jpeg => opaque()
            .write_with_encoder(JpegEncoder::new_with_quality(f, o.quality.unwrap_or(80)))
            .map_err(failed),
        InputFormat::Webp => {
            let (w, h) = (img.width(), img.height());
            let (rgba, rgb);
            let encoder = if img.color().has_alpha() {
                rgba = img.to_rgba8();
                webp::Encoder::from_rgba(&rgba, w, h)
            } else {
                rgb = img.to_rgb8();
                webp::Encoder::from_rgb(&rgb, w, h)
            };
            let memory = if o.lossless {
                encoder.encode_lossless()
            } else {
                encoder.encode(f32::from(o.quality.unwrap_or(80)))
            };
            f.write_all(&memory).map_err(internal)
        }
        InputFormat::Avif => {
            let speed = 10u8.saturating_sub(o.effort.unwrap_or(4)).clamp(1, 10);
            let quality = if o.lossless {
                100
            } else {
                o.quality.unwrap_or(50)
            };
            DynamicImage::ImageRgba8(img.to_rgba8())
                .write_with_encoder(::image::codecs::avif::AvifEncoder::new_with_speed_quality(
                    f, speed, quality,
                ))
                .map_err(failed)
        }
        InputFormat::Gif => GifEncoder::new(f)
            .encode_frame(Frame::new(img.to_rgba8()))
            .map_err(failed),
        InputFormat::Png | InputFormat::Tiff | InputFormat::Bmp | InputFormat::Ico => {
            img.write_to(f, codec(format)).map_err(failed)
        }
    }
}

/// `img` without the edges that match `t.background` (else its top-left pixel) to within
/// `t.threshold` (else 10) on every channel. A picture that's all edge stays as it is.
pub fn trim(img: DynamicImage, t: &TrimOptions) -> DynamicImage {
    let rgba = img.to_rgba8();
    let bg = t
        .background
        .as_deref()
        .and_then(hex_colour)
        .unwrap_or_else(|| *rgba.get_pixel(0, 0));
    let threshold = t.threshold.unwrap_or(10);
    let differs = |p: &Rgba<u8>| p.0.iter().zip(bg.0).any(|(a, b)| a.abs_diff(b) > threshold);
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in rgba.enumerate_pixels() {
        if differs(p) {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    if x0 == u32::MAX {
        return img;
    }
    img.crop_imm(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
}

/// `#rgb` or `#rrggbb`, opaque.
fn hex_colour(s: &str) -> Option<Rgba<u8>> {
    let hex = s.strip_prefix('#')?;
    let digit = |i: usize, n: usize| u8::from_str_radix(hex.get(i..i + n)?, 16).ok();
    match hex.len() {
        3 => Some(Rgba([
            digit(0, 1)? * 17,
            digit(1, 1)? * 17,
            digit(2, 1)? * 17,
            255,
        ])),
        6 => Some(Rgba([digit(0, 2)?, digit(2, 2)?, digit(4, 2)?, 255])),
        _ => None,
    }
}

/// Where `p` keeps a crop, from 0 (left, top) to 1 (right, bottom).
fn anchor(p: Position) -> (f64, f64) {
    match p {
        Position::Centre => (0.5, 0.5),
        Position::Top => (0.5, 0.0),
        Position::RightTop => (1.0, 0.0),
        Position::Right => (1.0, 0.5),
        Position::RightBottom => (1.0, 1.0),
        Position::Bottom => (0.5, 1.0),
        Position::LeftBottom => (0.0, 1.0),
        Position::Left => (0.0, 0.5),
        Position::LeftTop => (0.0, 0.0),
    }
}

fn round(v: f64) -> u32 {
    (v.round() as u32).max(1)
}

/// `img` resized to `width` × `height` the way `fit` says; with one side only, the other
/// keeps the aspect ratio. `without_enlargement` leaves an image that would grow as it is.
pub fn resize(
    img: &DynamicImage,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    position: Position,
    without_enlargement: bool,
) -> Result<DynamicImage, ImageError> {
    let (iw, ih) = (f64::from(img.width()), f64::from(img.height()));
    let (w, h) = match (width, height) {
        (None, None) => return Ok(img.clone()),
        (Some(w), None) => (f64::from(w), (ih * f64::from(w) / iw)),
        (None, Some(h)) => (iw * f64::from(h) / ih, f64::from(h)),
        (Some(w), Some(h)) => (f64::from(w), f64::from(h)),
    };
    let one_side = width.is_none() || height.is_none();
    let (sx, sy) = (w / iw, h / ih);
    let grows = match fit {
        _ if one_side => sx > 1.0,
        Fit::Fill => sx > 1.0 || sy > 1.0,
        Fit::Cover | Fit::Outside => sx.max(sy) > 1.0,
        Fit::Contain | Fit::Inside => sx.min(sy) > 1.0,
    };
    if without_enlargement && grows {
        return Ok(img.clone());
    }
    let (ax, ay) = anchor(position);
    if one_side || fit == Fit::Fill {
        return scale(img, round(w), round(h), None);
    }
    match fit {
        Fit::Cover => {
            let (cw, ch) = (iw.min(ih * w / h), ih.min(iw * h / w));
            let crop = ((iw - cw) * ax, (ih - ch) * ay, cw, ch);
            scale(img, round(w), round(h), Some(crop))
        }
        Fit::Outside => {
            let s = sx.max(sy);
            scale(img, round(iw * s), round(ih * s), None)
        }
        Fit::Inside => {
            let s = sx.min(sy);
            scale(img, round(iw * s), round(ih * s), None)
        }
        Fit::Contain | Fit::Fill => {
            let s = sx.min(sy);
            let inner = scale(img, round(iw * s), round(ih * s), None)?;
            let (w, h) = (round(w), round(h));
            let mut canvas = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
            let x = (f64::from(w - inner.width()) * ax).round() as i64;
            let y = (f64::from(h - inner.height()) * ay).round() as i64;
            ::image::imageops::overlay(&mut canvas, &inner.to_rgba8(), x, y);
            Ok(DynamicImage::ImageRgba8(canvas))
        }
    }
}

/// `img`, or the `crop` of it, scaled to `w` × `h`.
fn scale(
    img: &DynamicImage,
    w: u32,
    h: u32,
    crop: Option<(f64, f64, f64, f64)>,
) -> Result<DynamicImage, ImageError> {
    let mut out = DynamicImage::new(w, h, img.color());
    let mut options = Fir::new();
    if let Some((left, top, cw, ch)) = crop {
        options = options.crop(left, top, cw, ch);
    }
    Resizer::new()
        .resize(img, &mut out, &options)
        .map_err(|e| ImageError::Internal(format!("can't resize an image: {e}")))?;
    Ok(out)
}

/// A 4×3 blurhash of `img`, from a copy 32 pixels wide.
pub fn blurhash(img: &DynamicImage) -> Option<String> {
    let h = (32.0 * f64::from(img.height()) / f64::from(img.width())).clamp(1.0, 128.0);
    let small = scale(img, 32, h.round() as u32, None).ok()?.to_rgba8();
    blurhash::encode(4, 3, small.width(), small.height(), small.as_raw()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{cmyk_jpeg, encoded, exif, gif, gradient, png_header, with_exif};
    use ::image::{GrayImage, Luma, RgbImage};
    use tachyon_core::uploads::{ConstructorOptions, FormatOptions, ResizeOptions};

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tachyon-image-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn upload(extra: serde_json::Value) -> Upload {
        let mut v = serde_json::json!({});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    /// `bytes` written to a new file in `d`.
    fn file(d: &Path, bytes: &[u8]) -> PathBuf {
        let p = d.join(format!("in-{}", uuid::Uuid::now_v7()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn exif_of(path: &Path) -> Option<Bytes> {
        let bytes = std::fs::read(path).unwrap();
        img_parts::DynImage::from_bytes(bytes.into())
            .unwrap()
            .and_then(|i| i.exif())
    }

    fn run(bytes: &[u8], u: &Upload) -> Result<Processed, ImageError> {
        let d = dir();
        let input = file(&d, bytes);
        let p = probe(&input, &bytes[..bytes.len().min(8192)])?;
        process_original(&input, &p, u, 50_000_000, &d)
    }

    #[test]
    fn a_bomb_is_refused_before_decoding() {
        let bomb = png_header(100_000, 100_000);
        assert!(bomb.len() < 5_000);
        assert_eq!(
            run(&bomb, &upload(serde_json::json!({}))),
            Err(ImageError::TooManyPixels {
                pixels: 10_000_000_000,
                cap: 50_000_000
            })
        );
        // The same file under the cap gets as far as decoding, which finds the junk.
        let small = png_header(100, 100);
        assert!(matches!(
            run(&small, &upload(serde_json::json!({}))),
            Err(ImageError::Truncated | ImageError::Corrupt(_))
        ));
    }

    #[test]
    fn the_cap_is_the_smaller_limit() {
        let u = upload(serde_json::json!({"constructorOptions": {"limitInputPixels": 1000}}));
        assert_eq!(cap(&u, 50_000_000), 1000);
        assert_eq!(cap(&u, 10), 10);
        assert_eq!(
            run(&encoded(&gradient(40, 30), Codec::Png), &u),
            Err(ImageError::TooManyPixels {
                pixels: 1200,
                cap: 1000
            })
        );
    }

    #[test]
    fn frames_count_toward_the_cap() {
        let d = dir();
        let fifty = file(&d, &gif(1000, 1000, 50));
        let p = probe(&fifty, &std::fs::read(&fifty).unwrap()).unwrap();
        assert_eq!((p.width, p.height, p.frames), (1000, 1000, 50));
        assert_eq!(check_cap(&p, 50_000_000), Ok(()));
        let more = file(&d, &gif(1000, 1000, 51));
        let p = probe(&more, &std::fs::read(&more).unwrap()).unwrap();
        assert_eq!(
            check_cap(&p, 50_000_000),
            Err(ImageError::TooManyPixels {
                pixels: 51_000_000,
                cap: 50_000_000
            })
        );
    }

    #[test]
    fn gif_comments_are_stripped_and_frames_kept() {
        let out = run(&gif(4, 4, 3), &upload(serde_json::json!({}))).unwrap();
        let bytes = std::fs::read(&out.path).unwrap();
        assert!(!bytes.windows(5).any(|w| w == b"hello"));
        assert_eq!(gif_blocks(&bytes, false).unwrap().0, 3);
        assert_eq!((out.width, out.height, out.mime), (4, 4, "image/gif"));
    }

    #[test]
    fn gps_is_stripped() {
        let jpeg = with_exif(encoded(&gradient(16, 8), Codec::Jpeg), exif(None, true));
        let out = run(&jpeg, &upload(serde_json::json!({}))).unwrap();
        assert_eq!(exif_of(&out.path), None);
        assert_eq!((out.width, out.height), (16, 8));
        assert!(out.blurhash.is_some());
        let kept = run(&jpeg, &upload(serde_json::json!({"withMetadata": true}))).unwrap();
        assert!(exif_of(&kept.path).is_some());
        assert_eq!(std::fs::read(&kept.path).unwrap(), jpeg);
    }

    #[test]
    fn whatever_follows_the_image_goes() {
        let mut jpeg = with_exif(encoded(&gradient(16, 8), Codec::Jpeg), exif(None, true));
        let trailing = jpeg.clone();
        jpeg.extend_from_slice(&trailing);
        let out = run(&jpeg, &upload(serde_json::json!({}))).unwrap();
        let bytes = std::fs::read(&out.path).unwrap();
        assert!(!bytes.windows(4).any(|w| w == b"Exif"));
    }

    #[test]
    fn png_and_webp_text_is_stripped() {
        let png = encoded(&gradient(8, 8), Codec::Png);
        let mut p = img_parts::png::Png::from_bytes(png.into()).unwrap();
        let at = p.chunks().len() - 1;
        p.chunks_mut().insert(
            at,
            img_parts::png::PngChunk::new(*b"tEXt", Bytes::from_static(b"GPS\0here")),
        );
        p.set_exif(Some(exif(None, true).into()));
        let out = run(&p.encoder().bytes(), &upload(serde_json::json!({}))).unwrap();
        let bytes = std::fs::read(&out.path).unwrap();
        assert!(!bytes.windows(4).any(|w| w == b"here" || w == b"eXIf"));

        let webp = encoded(&gradient(8, 8), Codec::WebP);
        let mut w = img_parts::webp::WebP::from_bytes(webp.into()).unwrap();
        w.set_exif(Some(exif(None, true).into()));
        let out = run(&w.encoder().bytes(), &upload(serde_json::json!({}))).unwrap();
        assert_eq!(exif_of(&out.path), None);
        assert_eq!((out.width, out.height), (8, 8));
    }

    #[test]
    fn orientation_survives_stripping() {
        let jpeg = with_exif(
            encoded(&gradient(400, 200), Codec::Jpeg),
            exif(Some(6), true),
        );
        let d = dir();
        let input = file(&d, &jpeg);
        let p = probe(&input, &jpeg).unwrap();
        assert_eq!((p.width, p.height, p.orientation), (400, 200, 6));
        let out =
            process_original(&input, &p, &upload(serde_json::json!({})), 50_000_000, &d).unwrap();
        assert_eq!((out.width, out.height), (200, 400));
        assert_eq!(exif_of(&out.path), None);
        let stored = ::image::load_from_memory(&std::fs::read(&out.path).unwrap()).unwrap();
        assert_eq!((stored.width(), stored.height()), (200, 400));
    }

    #[test]
    fn transforms_apply_in_order() {
        // A 60×40 red block in a 100×80 white frame.
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(100, 80, |x, y| {
            if (20..80).contains(&x) && (20..60).contains(&y) {
                ::image::Rgb([255, 0, 0])
            } else {
                ::image::Rgb([255, 255, 255])
            }
        }));
        let u = upload(serde_json::json!({
            "trimOptions": {},
            "resizeOptions": {"width": 30},
            "formatOptions": {"format": "webp"}
        }));
        let out = run(&encoded(&img, Codec::Png), &u).unwrap();
        assert_eq!((out.width, out.height, out.mime), (30, 20, "image/webp"));
        let stored = ::image::load_from_memory(&std::fs::read(&out.path).unwrap()).unwrap();
        assert_eq!((stored.width(), stored.height()), (30, 20));
        assert_eq!(
            ::image::guess_format(&std::fs::read(&out.path).unwrap()).unwrap(),
            Codec::WebP
        );
    }

    #[test]
    fn fits_follow_their_names() {
        let img = gradient(200, 100);
        let size = |w, h, fit| {
            let r = resize(&img, w, h, fit, Position::Centre, false).unwrap();
            (r.width(), r.height())
        };
        assert_eq!(size(Some(50), None, Fit::Cover), (50, 25));
        assert_eq!(size(None, Some(50), Fit::Cover), (100, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Cover), (50, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Contain), (50, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Fill), (50, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Inside), (50, 25));
        assert_eq!(size(Some(50), Some(50), Fit::Outside), (100, 50));
        let kept = resize(&img, Some(400), None, Fit::Cover, Position::Centre, true).unwrap();
        assert_eq!((kept.width(), kept.height()), (200, 100));
        // Cover keeps the side `position` names.
        let half = DynamicImage::ImageLuma8(GrayImage::from_fn(200, 100, |x, _| {
            Luma([if x < 100 { 0 } else { 255 }])
        }));
        let left = resize(&half, Some(50), Some(50), Fit::Cover, Position::Left, false).unwrap();
        assert!(left.to_luma8().pixels().all(|p| p.0[0] < 20));
        let right = resize(
            &half,
            Some(50),
            Some(50),
            Fit::Cover,
            Position::Right,
            false,
        )
        .unwrap();
        assert!(right.to_luma8().pixels().all(|p| p.0[0] > 235));
    }

    #[test]
    fn cmyk_jpeg_decodes() {
        let cmyk = cmyk_jpeg(24, 16);
        let out = run(&cmyk, &upload(serde_json::json!({}))).unwrap();
        assert_eq!((out.width, out.height), (24, 16));
        assert!(out.blurhash.is_some());
    }

    #[test]
    fn truncated_is_refused_unless_fail_on_is_none() {
        let png = encoded(&gradient(64, 64), Codec::Png);
        let cut = &png[..png.len() / 2];
        assert_eq!(
            run(cut, &upload(serde_json::json!({}))),
            Err(ImageError::Truncated)
        );
        let jpeg = encoded(&gradient(64, 64), Codec::Jpeg);
        let cut_jpeg = &jpeg[..jpeg.len() - 40];
        assert_eq!(
            run(cut_jpeg, &upload(serde_json::json!({}))),
            Err(ImageError::Truncated)
        );
        let lenient = upload(serde_json::json!({"constructorOptions": {"failOn": "none"}}));
        let out = run(cut_jpeg, &lenient).unwrap();
        assert_eq!((out.width, out.height), (64, 64));
        // A PNG cut inside a chunk can't be walked, so its metadata can't be taken out.
        assert!(matches!(run(cut, &lenient), Err(ImageError::Corrupt(_))));
    }

    #[test]
    fn tiff_is_reencoded_without_its_metadata_and_bmp_kept() {
        let tiff = encoded(&gradient(10, 6), Codec::Tiff);
        let out = run(&tiff, &upload(serde_json::json!({}))).unwrap();
        assert_eq!((out.width, out.height, out.mime), (10, 6, "image/tiff"));
        assert_ne!(out.path.file_name(), None);
        let bmp = encoded(&gradient(10, 6), Codec::Bmp);
        let d = dir();
        let input = file(&d, &bmp);
        let p = probe(&input, &bmp).unwrap();
        let out =
            process_original(&input, &p, &upload(serde_json::json!({})), 50_000_000, &d).unwrap();
        assert_eq!(out.path, input);
        assert_eq!((out.width, out.height, out.mime), (10, 6, "image/bmp"));
    }

    #[test]
    fn avif_decodes() {
        let avif = encoded(&gradient(20, 12), Codec::Avif);
        let d = dir();
        let input = file(&d, &avif);
        let p = probe(&input, &avif).unwrap();
        assert_eq!((p.width, p.height, p.format), (20, 12, InputFormat::Avif));
        let out =
            process_original(&input, &p, &upload(serde_json::json!({})), 50_000_000, &d).unwrap();
        assert_eq!((out.width, out.height, out.mime), (20, 12, "image/avif"));
        assert!(out.blurhash.is_some());
    }

    #[test]
    fn heic_is_named() {
        let mut heic = vec![0, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic\0\0\0\0mif1heic");
        let d = dir();
        assert_eq!(
            probe(&file(&d, &heic), &heic),
            Err(ImageError::Unsupported("HEIC isn't supported yet"))
        );
    }

    #[test]
    fn format_options_reach_the_encoder() {
        let u = Upload {
            resize_options: Some(ResizeOptions {
                width: Some(8),
                ..ResizeOptions::default()
            }),
            format_options: Some(FormatOptions {
                format: ImageFormat::Jpeg,
                options: EncodeOptions {
                    quality: Some(10),
                    ..EncodeOptions::default()
                },
            }),
            constructor_options: Some(ConstructorOptions::default()),
            ..upload(serde_json::json!({}))
        };
        let out = run(&encoded(&gradient(16, 16), Codec::Png), &u).unwrap();
        assert_eq!((out.width, out.height, out.mime), (8, 8, "image/jpeg"));
    }
}
