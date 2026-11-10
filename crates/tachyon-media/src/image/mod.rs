//! What's inside an uploaded image: its size, read from the headers before anything is
//! decoded, and the original as it's stored, with its metadata gone, turned upright, and
//! trimmed, resized and re-encoded as the collection says.
//!
//! Every pixel buffer is made after `probe` has said how big it can be, and the decoders
//! are held to the same cap, so a small file claiming a huge image is refused unread.
//! Header reads are held to `HEADER_ALLOC`, so metadata can't be inflated without bound.

mod avif;
mod gif;
mod ico;
mod jpeg;
mod sizes;
#[cfg(test)]
mod walker_fuzz;

use std::io::{BufRead, BufReader, Cursor, Seek, Write};
use std::path::{Path, PathBuf};

use ::image::codecs::gif::{GifEncoder, Repeat};
use ::image::codecs::png::PngEncoder;
use ::image::codecs::tiff::TiffEncoder;
use ::image::codecs::webp::WebPDecoder;
use ::image::{
    AnimationDecoder, Delay, DynamicImage, Frame, GenericImageView, ImageDecoder, ImageEncoder,
    ImageFormat as Codec, ImageReader, Limits, Rgba, RgbaImage, metadata::Orientation,
};
use fast_image_resize::{ResizeOptions as Fir, Resizer};
use img_parts::{Bytes, ImageEXIF, ImageICC};
use tachyon_core::uploads::{
    EncodeOptions, FailOn, Fit, ImageFormat, Position, TrimOptions, Upload,
};

pub use sizes::{Made, Original, SizeOut, size_format};

/// The most a header read may allocate: ICC profiles and text chunks, never pixels.
const HEADER_ALLOC: u64 = 16 * 1024 * 1024;

/// The PNG chunks kept when metadata is stripped: what decoding and colour need.
const PNG_KEEP: &[&[u8; 4]] = &[
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB",
    b"cICP", b"mDCv", b"cLLi", b"bKGD", b"hIST", b"pHYs", b"sPLT", b"acTL", b"fcTL", b"fdAT",
];

const XMP_JPEG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

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
    /// The size as coded, before `orientation` turns it.
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    /// The EXIF orientation, 1 to 8; 1 when there's none. An AVIF's `irot` and `imir` are
    /// given as the orientation that does the same.
    pub orientation: u8,
    pub format: InputFormat,
    /// The most pixels decoding may make, all frames counted: what the cap is held to.
    pub pixels: u64,
    /// Whether the metadata can only be removed by re-encoding: a TIFF, or an AVIF whose
    /// metadata items can't all be located.
    pub strip_by_encoding: bool,
}

/// The original as it's to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    /// The input itself when it's stored as uploaded, else a new file in `out_dir`.
    pub path: PathBuf,
    pub mime: &'static str,
    /// The size as shown, turned upright.
    pub width: u32,
    pub height: u32,
    pub filesize: u64,
    pub blurhash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// From the headers, before any decoding.
    #[error("this image is {pixels} pixels, all its frames counted; the limit is {cap}")]
    TooManyPixels { pixels: u64, cap: u64 },
    /// From a decoder held to the limit.
    #[error("this image needs more memory to decode than the limit allows")]
    OverMemory,
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

#[cfg(test)]
thread_local! {
    /// Decodes this thread has started, so a test can show one never began.
    pub(crate) static DECODES_STARTED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn decode_started() {
    #[cfg(test)]
    DECODES_STARTED.with(|c| c.set(c.get() + 1));
}

/// `process_original` calls running now, and the most there have been in this process:
/// counted apart from any permit, so a test can see how many really ran at once.
static WORKING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static WORKING_PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The most `process_original` calls that have run at once.
#[doc(hidden)]
pub fn working_peak() -> usize {
    WORKING_PEAK.load(std::sync::atomic::Ordering::Relaxed)
}

/// `process_original` calls running now.
#[doc(hidden)]
pub fn working_now() -> usize {
    WORKING.load(std::sync::atomic::Ordering::Relaxed)
}

struct Working;

impl Working {
    fn start() -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let now = WORKING.fetch_add(1, Relaxed) + 1;
        WORKING_PEAK.fetch_max(now, Relaxed);
        Working
    }
}

impl Drop for Working {
    fn drop(&mut self) {
        WORKING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
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
    if p.pixels > cap {
        return Err(ImageError::TooManyPixels {
            pixels: p.pixels,
            cap,
        });
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
        ::image::ImageError::Limits(_) => ImageError::OverMemory,
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
/// headers: no pixel buffer is made, and metadata is read within `HEADER_ALLOC`.
pub fn probe(path: &Path, head: &[u8]) -> Result<Probe, ImageError> {
    unpanicked(|| probe_unguarded(path, head))
}

// The guard below catches panics only while they unwind: a profile, `.cargo/config.toml`
// or `RUSTFLAGS` setting `panic = "abort"` would turn a crafted file into a crash.
#[cfg(not(panic = "unwind"))]
compile_error!("tachyon-media needs panic = \"unwind\": its decoders run under catch_unwind");

/// `work`, a panic in it (a parser or decoder bug some file reaches) taken as a file that
/// can't be read rather than a failed request. Nothing here starts a thread, so every
/// panic unwinds through this.
fn unpanicked<T>(work: impl FnOnce() -> Result<T, ImageError>) -> Result<T, ImageError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(ImageError::Corrupt("decoding it failed".into())))
}

fn probe_unguarded(path: &Path, head: &[u8]) -> Result<Probe, ImageError> {
    let format = match avif::brand(head) {
        Some(avif::Brand::Avif) => InputFormat::Avif,
        Some(avif::Brand::AnimatedAvif) => {
            return Err(ImageError::Unsupported("animated AVIF isn't supported yet"));
        }
        Some(avif::Brand::Other) => {
            return Err(ImageError::Unsupported("HEIC isn't supported yet"));
        }
        None => match ::image::guess_format(head) {
            Ok(Codec::Jpeg) => InputFormat::Jpeg,
            Ok(Codec::Png) => InputFormat::Png,
            Ok(Codec::WebP) => InputFormat::Webp,
            Ok(Codec::Gif) => InputFormat::Gif,
            Ok(Codec::Tiff) => InputFormat::Tiff,
            Ok(Codec::Bmp) => InputFormat::Bmp,
            Ok(Codec::Ico) => InputFormat::Ico,
            _ => return Err(ImageError::Unsupported("this image format isn't supported")),
        },
    };
    let read = || std::fs::read(path).map_err(internal);
    let mut p = Probe {
        width: 0,
        height: 0,
        frames: 1,
        orientation: 1,
        format,
        pixels: 0,
        strip_by_encoding: format == InputFormat::Tiff,
    };
    match format {
        InputFormat::Avif => {
            let bytes = read()?;
            let parsed = avif::parse(&bytes)?;
            let (w, h) = avif::decode_bound(&parsed.primary_item)?;
            let (aw, ah) = match &parsed.alpha_item {
                Some(a) => avif::decode_bound(a)?,
                None => (0, 0),
            };
            let pixels = |w: u32, h: u32| u64::from(w) * u64::from(h);
            (p.width, p.height) = (w.max(aw), h.max(ah));
            p.pixels = pixels(w, h).max(pixels(aw, ah));
            let meta = avif::meta(&bytes)?;
            p.orientation = meta.orientation;
            // Metadata that can't be found can't be blanked: the picture is re-encoded.
            p.strip_by_encoding = meta.unresolved;
        }
        InputFormat::Gif => {
            let walked = gif::walk(&read()?, false)?;
            (p.width, p.height, p.frames, p.pixels) =
                (walked.width, walked.height, walked.frames, walked.pixels);
        }
        InputFormat::Ico => {
            let bytes = read()?;
            let icon = ico::walk(&bytes)?;
            let e = icon.largest().ok_or_else(no_icon)?;
            // Only the entry's declared bytes are read, as decoding reads them.
            let slice = bytes.get(e.start..e.end).ok_or_else(no_icon)?;
            (p.width, p.height) = if e.is_png() {
                let (w, h, _, _) = headers(Cursor::new(slice), InputFormat::Png)?;
                (w, h)
            } else {
                (e.width, e.height)
            };
        }
        InputFormat::Webp | InputFormat::Png => {
            let bytes = read()?;
            let icc;
            (p.width, p.height, p.orientation, icc) = headers(Cursor::new(&bytes), format)?;
            // The PNG decoder drops a profile over `HEADER_ALLOC` without a word; the
            // file is refused instead, so a decode never inflates it with a larger limit.
            let has_iccp = || {
                let mut budget = usize::MAX;
                ico::png_chunks(&bytes, &mut budget).map(|c| c.iter().any(|(k, _, _)| k == b"iCCP"))
            };
            if format == InputFormat::Png && !icc && has_iccp()? {
                return Err(ImageError::Corrupt(
                    "its colour profile is broken or larger than allowed".into(),
                ));
            }
            if format == InputFormat::Webp {
                p.frames = webp_info(&bytes).frames;
            }
        }
        _ => {
            let file = BufReader::new(std::fs::File::open(path).map_err(internal)?);
            (p.width, p.height, p.orientation, _) = headers(file, format)?;
        }
    }
    if p.pixels == 0 {
        p.pixels = u64::from(p.width) * u64::from(p.height) * u64::from(p.frames);
    }
    Ok(p)
}

pub(super) fn no_icon() -> ImageError {
    ImageError::Corrupt("this icon holds no image".into())
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

/// Width, height, EXIF orientation and whether there's an ICC profile, from a decoder that
/// has read only the headers,
/// with no limit on the size (the caller checks that against the cap) and `HEADER_ALLOC`
/// on what the headers may inflate.
fn headers<R: BufRead + Seek>(
    r: R,
    format: InputFormat,
) -> Result<(u32, u32, u8, bool), ImageError> {
    let mut reader = ImageReader::with_format(r, codec(format));
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(HEADER_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| match e {
        ::image::ImageError::Limits(_) => {
            ImageError::Corrupt("its metadata is larger than allowed".into())
        }
        e => from_image(e),
    })?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().map_or(1, |o| o.to_exif()).clamp(1, 8);
    let icc = decoder.icc_profile().ok().flatten().is_some();
    Ok((w, h, orientation, icc))
}

/// What a WebP's chunks say.
struct WebpInfo {
    frames: u32,
    lossless: bool,
    /// `None` for forever.
    loops: Option<u16>,
}

fn webp_info(bytes: &[u8]) -> WebpInfo {
    let mut info = WebpInfo {
        frames: 0,
        lossless: false,
        loops: None,
    };
    let mut i = 12;
    while let Some(h) = bytes.get(i..i + 8) {
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        match &h[..4] {
            b"ANMF" => info.frames = info.frames.saturating_add(1),
            b"VP8L" => info.lossless = true,
            b"ANIM" => {
                info.loops = bytes
                    .get(i + 12..i + 14)
                    .map(|l| u16::from_le_bytes([l[0], l[1]]))
                    .filter(|&n| n != 0);
            }
            _ => {}
        }
        i = i
            .saturating_add(8)
            .saturating_add(len)
            .saturating_add(len & 1);
    }
    info.frames = info.frames.max(1);
    info
}

/// A frame as it's shown, and for how long.
#[derive(Clone)]
struct Shown {
    image: DynamicImage,
    delay_ms: u32,
}

/// The metadata carried onto a re-encoded file.
#[derive(Default)]
struct Meta {
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
}

/// An image decoded: its frames (one unless all were asked for), upright, and what's kept.
struct Decoded {
    frames: Vec<Shown>,
    /// `None` for forever.
    loops: Option<u16>,
    meta: Meta,
}

/// `bytes` decoded, every frame when `all`, with at most `cap` pixels; the ICC profile
/// always, EXIF and XMP when `keep`.
fn decode(bytes: &[u8], p: &Probe, cap: u64, all: bool, keep: bool) -> Result<Decoded, ImageError> {
    decode_started();
    let side = u32::try_from(cap).unwrap_or(u32::MAX);
    let mut limits = Limits::default();
    limits.max_image_width = Some(side);
    limits.max_image_height = Some(side);
    // Eight bytes a pixel: 16-bit RGBA, the widest a decoder here makes.
    limits.max_alloc = Some(cap.saturating_mul(8));
    let mut meta = Meta::default();
    let (mut frames, loops) = match p.format {
        InputFormat::Avif => {
            let mut img = avif(&avif::as_avif(bytes), p.pixels)?;
            // A backstop: the probe bounded what the decoder could make.
            if img.width() > p.width || img.height() > p.height {
                return Err(ImageError::Corrupt(
                    "the AVIF decodes larger than its header says".into(),
                ));
            }
            let m = avif::meta(bytes)?;
            // The clean aperture applies before `irot` and `imir`.
            if let Some((x, y, w, h)) = m.clap.and_then(|c| c.rect(img.width(), img.height())) {
                img = img.crop_imm(x, y, w, h);
            }
            meta.icc = m.icc.and_then(|(s, e)| bytes.get(s..e)).map(<[u8]>::to_vec);
            if keep {
                (meta.exif, meta.xmp) = m.exif_and_xmp(bytes);
            }
            (
                vec![Shown {
                    image: img,
                    delay_ms: 0,
                }],
                None,
            )
        }
        InputFormat::Gif => {
            // Four bytes a pixel, for each frame the decoder holds.
            let (shown, loops) = gif::frames(bytes, all, cap.saturating_mul(4))?;
            let walked = gif::walk(bytes, false)?;
            meta.icc = walked.icc;
            if keep {
                meta.xmp = walked.xmp;
            }
            let frames = shown
                .into_iter()
                .map(|s| Shown {
                    image: DynamicImage::ImageRgba8(s.image),
                    delay_ms: s.delay_ms,
                })
                .collect();
            (frames, loops)
        }
        InputFormat::Webp if all && p.frames > 1 => {
            let mut d = WebPDecoder::new(Cursor::new(bytes)).map_err(from_image)?;
            d.set_limits(limits).map_err(from_image)?;
            meta.icc = d.icc_profile().ok().flatten();
            if keep {
                meta.exif = d.exif_metadata().ok().flatten();
                meta.xmp = d.xmp_metadata().ok().flatten();
            }
            let frames = d
                .into_frames()
                .map(|f| {
                    let f = f.map_err(from_image)?;
                    let (n, d) = f.delay().numer_denom_ms();
                    Ok(Shown {
                        delay_ms: n / d.max(1),
                        image: DynamicImage::ImageRgba8(f.into_buffer()),
                    })
                })
                .collect::<Result<Vec<_>, ImageError>>()?;
            (frames, webp_info(bytes).loops)
        }
        InputFormat::Ico => {
            let icon = ico::walk(bytes)?;
            let e = icon.largest().ok_or_else(no_icon)?;
            let slice = bytes.get(e.start..e.end).ok_or_else(no_icon)?;
            let img = if e.is_png() {
                let mut reader = ImageReader::with_format(Cursor::new(slice), Codec::Png);
                reader.limits(limits);
                let mut decoder = reader.into_decoder().map_err(from_image)?;
                meta.icc = decoder.icc_profile().ok().flatten();
                DynamicImage::from_decoder(decoder).map_err(from_image)?
            } else {
                DynamicImage::ImageRgba8(ico::decode_bitmap(slice, cap)?)
            };
            (
                vec![Shown {
                    image: img,
                    delay_ms: 0,
                }],
                None,
            )
        }
        _ => {
            let mut reader = ImageReader::with_format(Cursor::new(bytes), codec(p.format));
            reader.limits(limits);
            let mut decoder = reader.into_decoder().map_err(from_image)?;
            meta.icc = decoder.icc_profile().ok().flatten();
            if keep {
                meta.exif = decoder.exif_metadata().ok().flatten();
                meta.xmp = decoder.xmp_metadata().ok().flatten();
            }
            let img = DynamicImage::from_decoder(decoder).map_err(from_image)?;
            (
                vec![Shown {
                    image: img,
                    delay_ms: 0,
                }],
                None,
            )
        }
    };
    if let Some(o) = Orientation::from_exif(p.orientation) {
        for f in &mut frames {
            f.image.apply_orientation(o);
        }
    }
    Ok(Decoded {
        frames,
        loops,
        meta,
    })
}

/// `bytes` decoded on this thread, rav1d refusing a frame over `pixels`.
fn avif(bytes: &[u8], pixels: u64) -> Result<DynamicImage, ImageError> {
    use avif_decode::Image;
    let image = avif::guarded(|| {
        let limit = u32::try_from(pixels).unwrap_or(u32::MAX).max(1);
        avif_decode::Decoder::from_avif_limited(bytes, limit)
            .and_then(|d| d.to_image())
            .map_err(|e| match e {
                avif_decode::Error::Avif(e) => avif_error(e),
                e => corrupt(e),
            })
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

/// The original `input`, as `probe` read it, made into what `u` stores.
///
/// Without `withMetadata`, EXIF, XMP, IPTC and comments are stripped at container level,
/// the picture untouched, except where they can't be: a TIFF, an icon with metadata in
/// its PNG, and a JPEG, PNG or WebP whose orientation isn't 1, which is turned upright and
/// re-encoded (call 9). An AVIF keeps its `irot`/`imir`, which browsers apply.
///
/// `trimOptions`, `resizeOptions` and `formatOptions` apply in that order, to every frame
/// of an animation. A re-encoded file keeps its ICC profile, and with `withMetadata` its
/// EXIF (orientation reset to 1) and XMP, where the format holds them.
///
/// `failOn`: the decoders here report no warnings, so `warning`, `error` and `truncated`
/// refuse the same files, those cut off or broken; `none` stores them as they are where
/// their metadata can still be removed.
pub fn process_original(
    input: &Path,
    probe: &Probe,
    u: &Upload,
    max_pixels: u64,
    out_dir: &Path,
) -> Result<Processed, ImageError> {
    let _working = Working::start();
    unpanicked(|| process_unguarded(input, probe, u, max_pixels, out_dir))
}

fn process_unguarded(
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
    let walked_jpeg = match probe.format {
        InputFormat::Jpeg => Some(jpeg::walk(&bytes)?),
        _ => None,
    };
    let walked_gif = match probe.format {
        InputFormat::Gif => Some(gif::walk(&bytes, !u.with_metadata)?),
        _ => None,
    };
    let ends = walked_jpeg.as_ref().map(|j| j.ends);
    if strict && (ends == Some(false) || walked_gif.as_ref().is_some_and(|g| !g.ends)) {
        return Err(ImageError::Truncated);
    }
    let transforms =
        u.trim_options.is_some() || u.resize_options.is_some() || u.format_options.is_some();
    let upright = !u.with_metadata
        && probe.orientation != 1
        && matches!(
            probe.format,
            InputFormat::Jpeg | InputFormat::Png | InputFormat::Webp | InputFormat::Tiff
        );
    let must_encode = !u.with_metadata && probe.strip_by_encoding;
    let encode = transforms || upright || must_encode;
    let all = encode && probe.frames > 1;
    let decoded = match decode(&bytes, probe, cap, all, u.with_metadata) {
        Ok(d) => Some(d),
        Err(e @ (ImageError::TooManyPixels { .. } | ImageError::OverMemory)) => return Err(e),
        Err(e) if strict => return Err(e),
        // `failOn: none`: stored without what a decode would give.
        Err(_) if must_encode => {
            return Err(ImageError::Corrupt(
                "its metadata can't be removed without decoding it".into(),
            ));
        }
        Err(_) => None,
    };
    let decoded = match (encode, decoded) {
        (true, Some(d)) => return transform_and_encode(d, &bytes, probe, u, max_pixels, out_dir),
        (_, d) => d,
    };
    let stripped = if u.with_metadata {
        None
    } else {
        match probe.format {
            InputFormat::Jpeg => walked_jpeg.map(|j| j.stripped),
            InputFormat::Gif => walked_gif.map(|g| g.stripped),
            InputFormat::Avif => avif::strip(&bytes, &avif::meta(&bytes)?),
            InputFormat::Ico => ico::strip(&bytes, &ico::walk(&bytes)?),
            f => strip(f, &bytes)?,
        }
        .filter(|s| s[..] != bytes[..])
    };
    let first = decoded.as_ref().and_then(|d| d.frames.first());
    // Worked out before the file is written, so nothing after it can fail and leave it.
    let hash = first.and_then(|f| blurhash(&f.image));
    let (width, height) = first.map_or_else(
        || {
            if probe.orientation >= 5 {
                (probe.height, probe.width)
            } else {
                (probe.width, probe.height)
            }
        },
        |f| (f.image.width(), f.image.height()),
    );
    let (path, filesize) = match stripped {
        Some(b) => write_new(out_dir, &b)?,
        None => (input.to_path_buf(), bytes.len() as u64),
    };
    Ok(Processed {
        path,
        mime: mime_of(probe.format),
        width,
        height,
        filesize,
        blurhash: hash,
    })
}

/// `d` trimmed, resized and encoded as `u` says, into a new file in `out_dir`.
fn transform_and_encode(
    d: Decoded,
    bytes: &[u8],
    probe: &Probe,
    u: &Upload,
    max_pixels: u64,
    out_dir: &Path,
) -> Result<Processed, ImageError> {
    let Decoded {
        mut frames,
        loops,
        meta,
    } = d;
    if let Some(t) = &u.trim_options {
        let bg = trim_background(&frames[0].image, t);
        let threshold = t.threshold.unwrap_or(10);
        let boxes: Vec<_> = frames
            .iter()
            .filter_map(|f| bounds(&f.image, bg, threshold))
            .collect();
        if let Some(&(mut x0, mut y0, mut x1, mut y1)) = boxes.first() {
            for &(a, b, c, e) in &boxes {
                (x0, y0, x1, y1) = (x0.min(a), y0.min(b), x1.max(c), y1.max(e));
            }
            for f in &mut frames {
                f.image = f.image.crop_imm(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
            }
        }
    }
    // Every frame comes out the size of the first, so all of them together are held to
    // the cap before any but the first is made.
    let cap = cap(u, max_pixels);
    let count = frames.len() as u64;
    for (i, f) in frames.iter_mut().enumerate() {
        if let Some(r) = &u.resize_options {
            f.image = resize(
                &f.image,
                r.width,
                r.height,
                r.fit,
                r.position,
                r.without_enlargement.unwrap_or(false),
                cap,
            )?;
        }
        if i == 0 {
            let total = u64::from(f.image.width()) * u64::from(f.image.height());
            let total = total.saturating_mul(count);
            if total > cap {
                return Err(ImageError::Corrupt(format!(
                    "the transformed image would be {total} pixels, all its frames counted; \
                     the limit is {cap}"
                )));
            }
        }
    }
    let (out, options) = match &u.format_options {
        Some(f) => (made(f.format), f.options.clone()),
        None => (
            probe.format,
            EncodeOptions {
                // Turning a lossless WebP upright keeps it lossless.
                lossless: probe.format == InputFormat::Webp && webp_info(bytes).lossless,
                ..EncodeOptions::default()
            },
        ),
    };
    let meta = Meta {
        icc: meta.icc,
        exif: meta
            .exif
            .filter(|_| u.with_metadata)
            .map(|e| upright_exif(&e)),
        xmp: meta.xmp.filter(|_| u.with_metadata),
    };
    let first = &frames[0].image;
    let (width, height, hash) = (first.width(), first.height(), blurhash(first));
    let encoded = match out {
        InputFormat::Gif | InputFormat::Webp if frames.len() > 1 => {
            encode_animation(frames, loops, out, &options, &meta)?
        }
        _ => encode_to(&frames[0].image, out, &options, &meta)?,
    };
    let (path, filesize) = write_new(out_dir, &encoded)?;
    Ok(Processed {
        path,
        mime: mime_of(out),
        width,
        height,
        filesize,
        blurhash: hash,
    })
}

/// A new file in `dir` holding `bytes`, and its size. Nothing is left behind on failure.
fn write_new(dir: &Path, bytes: &[u8]) -> Result<(PathBuf, u64), ImageError> {
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(internal)?;
    file.write_all(bytes).map_err(internal)?;
    file.flush().map_err(internal)?;
    let (_, path) = file.keep().map_err(|e| internal(e.error))?;
    Ok((path, bytes.len() as u64))
}

/// `bytes` without EXIF, XMP, IPTC and comments, or `None` when it has no such container.
/// Colour profiles stay.
fn strip(format: InputFormat, bytes: &[u8]) -> Result<Option<Vec<u8>>, ImageError> {
    let b = Bytes::copy_from_slice(bytes);
    let out = match format {
        InputFormat::Png => {
            let mut png = img_parts::png::Png::from_bytes(b).map_err(corrupt)?;
            png.chunks_mut().retain(|c| PNG_KEEP.contains(&&c.kind()));
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
        _ => return Ok(None),
    };
    Ok(Some(out.to_vec()))
}

/// `exif` (the TIFF structure, with or without JPEG's `Exif\0\0`) with its orientation set
/// to 1, for a picture that's been turned upright.
fn upright_exif(exif: &[u8]) -> Vec<u8> {
    let mut out = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif).to_vec();
    let big = out.starts_with(b"MM");
    let u16_at = |b: &[u8], i: usize| {
        b.get(i..i + 2).map(|x| {
            if big {
                u16::from_be_bytes([x[0], x[1]])
            } else {
                u16::from_le_bytes([x[0], x[1]])
            }
        })
    };
    let ifd = out.get(4..8).map(|x| {
        let x = [x[0], x[1], x[2], x[3]];
        (if big {
            u32::from_be_bytes(x)
        } else {
            u32::from_le_bytes(x)
        }) as usize
    });
    let Some(ifd) = ifd else { return out };
    let Some(n) = u16_at(&out, ifd) else {
        return out;
    };
    for e in 0..usize::from(n) {
        let at = ifd + 2 + 12 * e;
        if u16_at(&out, at) == Some(0x0112) && out.len() >= at + 10 {
            let one = if big { [0, 1] } else { [1, 0] };
            out[at + 8..at + 10].copy_from_slice(&one);
        }
    }
    out
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

fn failed(e: impl std::fmt::Display) -> ImageError {
    ImageError::Internal(format!("can't encode an image: {e}"))
}

/// `img` encoded as `format` with `o`'s quality, losslessness, effort and progressive
/// where the format takes them, and `meta` where it holds it. Defaults follow the usual
/// ones: JPEG and WebP 80, AVIF 50, effort 4. AVIF re-encodes are 8-bit and carry the
/// profile and EXIF (ravif writes no XMP item); GIF carries the profile and XMP as
/// application extensions; TIFF carries the profile; BMP and ICO carry nothing.
fn encode_to(
    img: &DynamicImage,
    format: InputFormat,
    o: &EncodeOptions,
    meta: &Meta,
) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    match format {
        InputFormat::Jpeg => {
            let (data, colour) = if img.color().has_color() {
                (img.to_rgb8().into_raw(), jpeg_encoder::ColorType::Rgb)
            } else {
                (img.to_luma8().into_raw(), jpeg_encoder::ColorType::Luma)
            };
            let side = |v: u32| {
                u16::try_from(v)
                    .map_err(|_| ImageError::Corrupt(format!("a JPEG can't be {v} pixels across")))
            };
            let (w, h) = (side(img.width())?, side(img.height())?);
            let mut e = jpeg_encoder::Encoder::new(&mut out, o.quality.unwrap_or(80));
            e.set_progressive(o.progressive);
            if let Some(icc) = &meta.icc {
                e.add_icc_profile(icc).map_err(failed)?;
            }
            if let Some(exif) = &meta.exif {
                e.add_exif_metadata(exif).map_err(failed)?;
            }
            if let Some(xmp) = &meta.xmp {
                e.add_app_segment(1, [XMP_JPEG, xmp].concat())
                    .map_err(failed)?;
            }
            e.encode(&data, w, h, colour).map_err(failed)?;
        }
        InputFormat::Png => {
            let mut e = PngEncoder::new(&mut out);
            if let Some(icc) = &meta.icc {
                let _ = e.set_icc_profile(icc.clone());
            }
            if let Some(exif) = &meta.exif {
                let _ = e.set_exif_metadata(exif.clone());
            }
            img.write_with_encoder(e).map_err(failed)?;
            if let Some(xmp) = &meta.xmp {
                let mut png = img_parts::png::Png::from_bytes(out.into()).map_err(failed)?;
                let at = png.chunks().len() - 1;
                let mut text = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
                text.extend_from_slice(xmp);
                png.chunks_mut()
                    .insert(at, img_parts::png::PngChunk::new(*b"iTXt", text.into()));
                out = png.encoder().bytes().to_vec();
            }
        }
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
            let memory = encoder
                .encode_advanced(&webp_config(o)?)
                .map_err(|e| failed(format!("{e:?}")))?;
            out = webp_with_meta(&memory, meta)?;
        }
        InputFormat::Avif => {
            let speed = 10u8.saturating_sub(o.effort.unwrap_or(4)).clamp(1, 10);
            let quality = if o.lossless {
                100
            } else {
                o.quality.unwrap_or(50)
            };
            let mut e = ::image::codecs::avif::AvifEncoder::new_with_speed_quality(
                &mut out, speed, quality,
            );
            if let Some(exif) = &meta.exif {
                let _ = e.set_exif_metadata(exif.clone());
            }
            DynamicImage::ImageRgba8(img.to_rgba8())
                .write_with_encoder(e)
                .map_err(failed)?;
            // ravif writes no profile; it's added to the container after.
            if let Some(with) = meta
                .icc
                .as_deref()
                .and_then(|icc| avif::with_icc(&out, icc))
            {
                out = with;
            }
        }
        InputFormat::Gif => {
            GifEncoder::new(&mut out)
                .encode_frame(Frame::new(img.to_rgba8()))
                .map_err(failed)?;
            out = gif::with_meta(&out, meta.icc.as_deref(), meta.xmp.as_deref());
        }
        InputFormat::Tiff => {
            let mut cursor = Cursor::new(Vec::new());
            let mut e = TiffEncoder::new(&mut cursor);
            if let Some(icc) = &meta.icc {
                let _ = e.set_icc_profile(icc.clone());
            }
            img.write_with_encoder(e).map_err(failed)?;
            out = cursor.into_inner();
        }
        InputFormat::Bmp | InputFormat::Ico => {
            let mut cursor = Cursor::new(Vec::new());
            img.write_to(&mut cursor, codec(format)).map_err(failed)?;
            out = cursor.into_inner();
        }
    }
    Ok(out)
}

/// libwebp's settings for `o`: quality (80), lossless, and effort as its method (4).
fn webp_config(o: &EncodeOptions) -> Result<webp::WebPConfig, ImageError> {
    let mut c = webp::WebPConfig::new().map_err(|()| failed("libwebp's defaults"))?;
    c.quality = f32::from(o.quality.unwrap_or(80));
    c.lossless = i32::from(o.lossless);
    c.method = i32::from(o.effort.unwrap_or(4).min(6));
    Ok(c)
}

/// `frames` as an animated GIF or WebP, with their delays, `loops` and `meta` where the
/// format holds it (GIF: the profile and XMP; WebP: all three). Frames are handed over as
/// they are, RGBA already, not copied.
fn encode_animation(
    frames: Vec<Shown>,
    loops: Option<u16>,
    format: InputFormat,
    o: &EncodeOptions,
    meta: &Meta,
) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    if format == InputFormat::Gif {
        let mut e = GifEncoder::new(&mut out);
        e.set_repeat(loops.map_or(Repeat::Infinite, Repeat::Finite))
            .map_err(failed)?;
        e.encode_frames(frames.into_iter().map(|f| {
            let delay = Delay::from_numer_denom_ms(f.delay_ms, 1);
            Frame::from_parts(f.image.into_rgba8(), 0, 0, delay)
        }))
        .map_err(failed)?;
        drop(e);
        return Ok(gif::with_meta(
            &out,
            meta.icc.as_deref(),
            meta.xmp.as_deref(),
        ));
    }
    let delays: Vec<u32> = frames.iter().map(|f| f.delay_ms).collect();
    let rgba: Vec<RgbaImage> = frames.into_iter().map(|f| f.image.into_rgba8()).collect();
    let config = webp_config(o)?;
    let (w, h) = rgba[0].dimensions();
    let mut e = webp::AnimEncoder::new(w, h, &config);
    e.set_loop_count(loops.map_or(0, i32::from));
    let mut at = 0i32;
    for (img, delay) in rgba.iter().zip(delays) {
        e.add_frame(webp::AnimFrame::from_rgba(img, w, h, at));
        at = at.saturating_add(i32::try_from(delay).unwrap_or(i32::MAX));
    }
    let memory = e.try_encode().map_err(|e| failed(format!("{e:?}")))?;
    webp_with_meta(&memory, meta)
}

/// The WebP `bytes` with `meta`'s profile, EXIF and XMP as chunks.
fn webp_with_meta(bytes: &[u8], meta: &Meta) -> Result<Vec<u8>, ImageError> {
    let mut webp =
        img_parts::webp::WebP::from_bytes(Bytes::copy_from_slice(bytes)).map_err(failed)?;
    if let Some(xmp) = &meta.xmp {
        webp.chunks_mut().push(img_parts::riff::RiffChunk::new(
            *b"XMP ",
            img_parts::riff::RiffContent::Data(xmp.clone().into()),
        ));
    }
    webp.set_icc_profile(meta.icc.clone().map(Into::into));
    // Last: it also rewrites the VP8X flags to the chunks there are.
    webp.set_exif(meta.exif.clone().map(Into::into));
    Ok(webp.encoder().bytes().to_vec())
}

/// The colour `t` trims: its `background`, else `img`'s top-left pixel.
fn trim_background(img: &DynamicImage, t: &TrimOptions) -> Rgba<u8> {
    t.background
        .as_deref()
        .and_then(hex_colour)
        .unwrap_or_else(|| img.get_pixel(0, 0))
}

/// The smallest box (x0, y0, x1, y1, inclusive) holding every pixel of `img` that differs
/// from `bg` by more than `threshold` on a channel, or `None` when none does.
fn bounds(img: &DynamicImage, bg: Rgba<u8>, threshold: u8) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in img.pixels() {
        if p.0.iter().zip(bg.0).any(|(a, b)| a.abs_diff(b) > threshold) {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    (x0 != u32::MAX).then_some((x0, y0, x1, y1))
}

/// `img` without the edges that match `t.background` (else its top-left pixel) to within
/// `t.threshold` (else 10) on every channel. A picture that's all edge stays as it is.
pub fn trim(img: DynamicImage, t: &TrimOptions) -> DynamicImage {
    let bg = trim_background(&img, t);
    match bounds(&img, bg, t.threshold.unwrap_or(10)) {
        Some((x0, y0, x1, y1)) => img.crop_imm(x0, y0, x1 - x0 + 1, y1 - y0 + 1),
        None => img,
    }
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
/// A result over `limit` pixels is refused before it's made.
pub fn resize(
    img: &DynamicImage,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    position: Position,
    without_enlargement: bool,
    limit: u64,
) -> Result<DynamicImage, ImageError> {
    resize_around(
        img,
        width,
        height,
        fit,
        position,
        None,
        without_enlargement,
        limit,
    )
}

/// `resize`, a cover crop centred as near `focal` (0 to 1 across and down) as the picture
/// allows when given, instead of kept at `position`.
#[allow(clippy::too_many_arguments)]
fn resize_around(
    img: &DynamicImage,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    position: Position,
    focal: Option<(f64, f64)>,
    without_enlargement: bool,
    limit: u64,
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
    let fits = |ow: u32, oh: u32| {
        if u64::from(ow) * u64::from(oh) > limit {
            return Err(ImageError::Corrupt(format!(
                "resizing makes it {ow}×{oh}, over the limit of {limit} pixels"
            )));
        }
        Ok((ow, oh))
    };
    if one_side || fit == Fit::Fill {
        let (ow, oh) = fits(round(w), round(h))?;
        return scale(img, ow, oh, None);
    }
    match fit {
        Fit::Cover => {
            let (cw, ch) = (iw.min(ih * w / h), ih.min(iw * h / w));
            let (left, top) = match focal {
                Some((fx, fy)) => (
                    (fx * iw - cw / 2.0).clamp(0.0, iw - cw),
                    (fy * ih - ch / 2.0).clamp(0.0, ih - ch),
                ),
                None => ((iw - cw) * ax, (ih - ch) * ay),
            };
            let crop = (left, top, cw, ch);
            let (ow, oh) = fits(round(w), round(h))?;
            scale(img, ow, oh, Some(crop))
        }
        Fit::Outside => {
            let s = sx.max(sy);
            let (ow, oh) = fits(round(iw * s), round(ih * s))?;
            scale(img, ow, oh, None)
        }
        Fit::Inside => {
            let s = sx.min(sy);
            let (ow, oh) = fits(round(iw * s), round(ih * s))?;
            scale(img, ow, oh, None)
        }
        Fit::Contain | Fit::Fill => {
            let s = sx.min(sy);
            let (w, h) = fits(round(w), round(h))?;
            let inner = scale(img, round(iw * s), round(ih * s), None)?;
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
    use crate::fixtures::{
        avif_cropped, avif_rotated, avif_set_extent, avif_two_frames, avif_two_headers,
        avif_with_exif, cmyk_jpeg, encoded, exif, gif, gradient, iccp_bomb_png, ico_around, ico_of,
        ico_repeating, moving_gif, png_header, png_icc_turned, tiff_with_description, with_exif,
    };
    use ::image::{GrayImage, Luma, RgbImage};
    use tachyon_core::uploads::{ConstructorOptions, FormatOptions, ResizeOptions};

    /// A fresh directory, removed when the test's thread ends: what's made in it may
    /// outlive the helper that made it.
    fn dir() -> PathBuf {
        thread_local! {
            static DIRS: std::cell::RefCell<Vec<tempfile::TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let d = tempfile::Builder::new()
            .prefix("tachyon-image-")
            .tempdir()
            .unwrap();
        let path = d.path().to_owned();
        DIRS.with_borrow_mut(|dirs| dirs.push(d));
        path
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
        assert_eq!(gif::walk(&bytes, false).unwrap().frames, 3);
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
            let r = resize(&img, w, h, fit, Position::Centre, false, u64::MAX).unwrap();
            (r.width(), r.height())
        };
        assert_eq!(size(Some(50), None, Fit::Cover), (50, 25));
        assert_eq!(size(None, Some(50), Fit::Cover), (100, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Cover), (50, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Contain), (50, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Fill), (50, 50));
        assert_eq!(size(Some(50), Some(50), Fit::Inside), (50, 25));
        assert_eq!(size(Some(50), Some(50), Fit::Outside), (100, 50));
        let kept = resize(
            &img,
            Some(400),
            None,
            Fit::Cover,
            Position::Centre,
            true,
            u64::MAX,
        )
        .unwrap();
        assert_eq!((kept.width(), kept.height()), (200, 100));
        // Cover keeps the side `position` names.
        let half = DynamicImage::ImageLuma8(GrayImage::from_fn(200, 100, |x, _| {
            Luma([if x < 100 { 0 } else { 255 }])
        }));
        let left = resize(
            &half,
            Some(50),
            Some(50),
            Fit::Cover,
            Position::Left,
            false,
            u64::MAX,
        )
        .unwrap();
        assert!(left.to_luma8().pixels().all(|p| p.0[0] < 20));
        let right = resize(
            &half,
            Some(50),
            Some(50),
            Fit::Cover,
            Position::Right,
            false,
            u64::MAX,
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

    #[test]
    fn an_iccp_bomb_is_refused_in_probe() {
        let bomb = iccp_bomb_png(32 << 20);
        assert!(bomb.len() < 300_000, "{}", bomb.len());
        let d = dir();
        let got = probe(&file(&d, &bomb), &bomb);
        assert!(matches!(got, Err(ImageError::Corrupt(_))), "{got:?}");
    }

    #[test]
    fn an_iccp_bomb_inside_an_icon_is_refused() {
        // The plain icon is stored, so a refusal is the bomb's.
        let plain = ico_around(&encoded(&crate::fixtures::rgba(1, 1), Codec::Png));
        let ok = run(&plain, &upload(serde_json::json!({})));
        assert!(ok.is_ok(), "{ok:?}");
        let ico = ico_around(&iccp_bomb_png(32 << 20));
        let got = run(&ico, &upload(serde_json::json!({})));
        assert!(matches!(got, Err(ImageError::Corrupt(_))), "{got:?}");
    }

    #[test]
    fn a_second_avif_sequence_header_cant_lift_the_cap() {
        let avif = avif_two_headers(64, 64);
        let u = upload(serde_json::json!({"constructorOptions": {"limitInputPixels": 100}}));
        let got = run(&avif, &u);
        assert!(got.is_err(), "{:?}", got.map(|p| (p.width, p.height)));
    }

    fn json(v: serde_json::Value) -> Upload {
        upload(v)
    }

    fn stored(p: &Processed) -> Vec<u8> {
        std::fs::read(&p.path).unwrap()
    }

    fn has(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn the_bomb_never_reaches_a_decoder() {
        DECODES_STARTED.with(|c| c.set(0));
        let bomb = png_header(100_000, 100_000);
        assert!(matches!(
            run(&bomb, &json(serde_json::json!({}))),
            Err(ImageError::TooManyPixels { .. })
        ));
        assert_eq!(DECODES_STARTED.with(|c| c.get()), 0);
        // A decode held to a smaller cap says so in its own words.
        let png = encoded(&gradient(40, 30), Codec::Png);
        let d = dir();
        let input = file(&d, &png);
        let p = probe(&input, &png).unwrap();
        assert_eq!(
            decode(&png, &p, 10, false, false).err(),
            Some(ImageError::OverMemory)
        );
        assert_eq!(DECODES_STARTED.with(|c| c.get()), 1);
    }

    #[test]
    fn data_after_the_end_is_kept_out_not_refused() {
        let mut jpeg = encoded(&gradient(16, 8), Codec::Jpeg);
        let clean = jpeg.clone();
        jpeg.extend_from_slice(b"\0\0\0\x18ftypmp42 a motion photo's video");
        let out = run(&jpeg, &json(serde_json::json!({}))).unwrap();
        assert_eq!(stored(&out), clean);
    }

    #[test]
    fn animations_are_transformed_frame_by_frame() {
        let u = json(serde_json::json!({
            "resizeOptions": {"width": 2},
            "formatOptions": {"format": "webp"}
        }));
        // Three frames that differ, so no encoder merges them.
        let mut moving = Vec::new();
        {
            let mut e = GifEncoder::new(&mut moving);
            e.set_repeat(Repeat::Infinite).unwrap();
            for v in [0u8, 120, 240] {
                let img = RgbaImage::from_pixel(4, 4, Rgba([v, 255 - v, 0, 255]));
                e.encode_frame(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
            }
        }
        let out = run(&moving, &u).unwrap();
        assert_eq!((out.width, out.height, out.mime), (2, 2, "image/webp"));
        assert_eq!(webp_info(&stored(&out)).frames, 3);
        let as_gif = json(serde_json::json!({"resizeOptions": {"width": 2}}));
        let out = run(&moving, &as_gif).unwrap();
        let walked = gif::walk(&stored(&out), false).unwrap();
        assert_eq!((walked.width, walked.frames), (2, 3));
    }

    #[test]
    fn reencodes_keep_the_profile_and_metadata_when_asked() {
        let png = png_icc_turned(8, 4, 300);
        let out = run(&png, &json(serde_json::json!({}))).unwrap();
        assert_eq!((out.width, out.height), (4, 8));
        let kept = img_parts::png::Png::from_bytes(stored(&out).into()).unwrap();
        assert!(kept.icc_profile().is_some(), "the profile stays");
        assert_eq!(kept.exif(), None);

        let jpeg = with_exif(encoded(&gradient(8, 4), Codec::Jpeg), exif(Some(6), true));
        let u = json(serde_json::json!({"withMetadata": true, "resizeOptions": {"width": 2}}));
        let out = run(&jpeg, &u).unwrap();
        assert_eq!((out.width, out.height), (2, 4));
        let kept = img_parts::jpeg::Jpeg::from_bytes(stored(&out).into()).unwrap();
        let exif = kept.exif().expect("EXIF carried");
        let d = dir();
        let back = probe(&file(&d, &stored(&out)), &stored(&out)).unwrap();
        assert_eq!(back.orientation, 1, "reset after turning");
        assert!(has(&exif, &[0x88, 0x25]), "the rest of the EXIF stays");
    }

    #[test]
    fn progressive_jpeg_is_progressive() {
        let u = json(serde_json::json!({
            "formatOptions": {"format": "jpeg", "options": {"progressive": true}}
        }));
        let out = run(&encoded(&gradient(32, 32), Codec::Png), &u).unwrap();
        assert!(has(&stored(&out), &[0xFF, 0xC2]));
        let plain = json(serde_json::json!({"formatOptions": {"format": "jpeg"}}));
        let out = run(&encoded(&gradient(32, 32), Codec::Png), &plain).unwrap();
        assert!(!has(&stored(&out), &[0xFF, 0xC2]));
    }

    #[test]
    fn avif_rotation_is_applied_and_reported() {
        let avif = avif_rotated(20, 12, 1);
        let d = dir();
        let input = file(&d, &avif);
        let p = probe(&input, &avif).unwrap();
        assert_eq!((p.width, p.height, p.orientation), (20, 12, 8));
        let out =
            process_original(&input, &p, &json(serde_json::json!({})), 50_000_000, &d).unwrap();
        assert_eq!((out.width, out.height), (12, 20));
        let u = json(serde_json::json!({"formatOptions": {"format": "png"}}));
        let out = process_original(&input, &p, &u, 50_000_000, &d).unwrap();
        let made = ::image::load_from_memory(&stored(&out)).unwrap();
        assert_eq!((made.width(), made.height()), (12, 20));
    }

    #[test]
    fn avif_metadata_is_blanked_in_place() {
        let avif = avif_with_exif(20, 12, exif(None, true));
        assert!(has(&avif, b"N\0\0\0"));
        let out = run(&avif, &json(serde_json::json!({}))).unwrap();
        let kept = stored(&out);
        assert_eq!(kept.len(), avif.len(), "the picture untouched");
        assert!(!has(&kept, b"MM\0\x2a"), "the EXIF is gone");
        assert_eq!((out.width, out.height), (20, 12));
        let d = dir();
        let again = probe(&file(&d, &kept), &kept).unwrap();
        assert_eq!((again.width, again.height), (20, 12));
    }

    #[test]
    fn a_lossless_webp_stays_lossless_when_turned() {
        let webp = encoded(&gradient(8, 4), Codec::WebP);
        let mut w = img_parts::webp::WebP::from_bytes(webp.into()).unwrap();
        w.set_exif(Some(exif(Some(6), false).into()));
        let out = run(&w.encoder().bytes(), &json(serde_json::json!({}))).unwrap();
        assert_eq!((out.width, out.height), (4, 8));
        assert!(webp_info(&stored(&out)).lossless);
    }

    #[test]
    fn icon_metadata_is_reencoded_away() {
        let png = encoded(&crate::fixtures::rgba(4, 4), Codec::Png);
        let mut p = img_parts::png::Png::from_bytes(png.into()).unwrap();
        let at = p.chunks().len() - 1;
        p.chunks_mut().insert(
            at,
            img_parts::png::PngChunk::new(*b"tEXt", Bytes::from_static(b"GPS\0secret place")),
        );
        let ico = ico_around(&p.encoder().bytes());
        let out = run(&ico, &json(serde_json::json!({}))).unwrap();
        assert!(!has(&stored(&out), b"secret"));
        assert_eq!(
            (out.width, out.height, out.mime),
            (4, 4, "image/vnd.microsoft.icon")
        );
    }

    #[test]
    fn tiff_metadata_goes() {
        let tiff = tiff_with_description("GPS 51.5N secret");
        let d = dir();
        let input = file(&d, &tiff);
        let p = probe(&input, &tiff).unwrap();
        let out =
            process_original(&input, &p, &json(serde_json::json!({})), 50_000_000, &d).unwrap();
        assert_ne!(out.path, input);
        assert!(!has(&stored(&out), b"secret"));
        assert_eq!((out.width, out.height, out.mime), (2, 2, "image/tiff"));
    }

    #[test]
    fn quality_and_effort_reach_the_encoders() {
        let img = encoded(&gradient(64, 64), Codec::Png);
        let size = |format: &str, options: serde_json::Value| {
            let u =
                json(serde_json::json!({"formatOptions": {"format": format, "options": options}}));
            run(&img, &u).unwrap().filesize
        };
        assert!(
            size("jpeg", serde_json::json!({"quality": 10}))
                < size("jpeg", serde_json::json!({"quality": 95}))
        );
        assert!(
            size("webp", serde_json::json!({"quality": 10}))
                < size("webp", serde_json::json!({"quality": 95}))
        );
        assert_ne!(
            size("webp", serde_json::json!({"effort": 0})),
            size("webp", serde_json::json!({"effort": 6}))
        );
    }

    #[test]
    fn gif_frames_bigger_than_the_screen_count() {
        let mut g = gif(100, 100, 1);
        g[6..10].copy_from_slice(&[1, 0, 1, 0]);
        let d = dir();
        let p = probe(&file(&d, &g), &g).unwrap();
        assert_eq!((p.width, p.height, p.pixels), (1, 1, 10_000));
    }

    #[test]
    fn a_big_gif_decodes_within_the_cap() {
        let out = run(&gif(4000, 4000, 1), &json(serde_json::json!({}))).unwrap();
        assert_eq!((out.width, out.height), (4000, 4000));
    }

    #[test]
    fn a_gif_without_its_trailer_is_strict_mode_only() {
        let mut g = gif(4, 4, 2);
        g.pop();
        assert_eq!(
            run(&g, &json(serde_json::json!({}))),
            Err(ImageError::Truncated)
        );
        let lenient = json(serde_json::json!({"constructorOptions": {"failOn": "none"}}));
        let out = run(&g, &lenient).unwrap();
        assert_eq!((out.width, out.height), (4, 4));
    }

    #[test]
    fn an_avif_branded_mif1_is_an_avif() {
        let mut avif = encoded(&gradient(20, 12), Codec::Avif);
        // The major brand `mif1`, with `avif` among the compatible ones.
        avif[8..12].copy_from_slice(b"mif1");
        avif[16..20].copy_from_slice(b"avif");
        let d = dir();
        let p = probe(&file(&d, &avif), &avif).unwrap();
        assert_eq!(p.format, InputFormat::Avif);
    }

    #[test]
    fn a_resize_over_the_limit_is_refused() {
        let img = gradient(10, 10);
        let got = resize(
            &img,
            Some(60_000),
            None,
            Fit::Cover,
            Position::Centre,
            false,
            1_000_000,
        );
        assert!(matches!(got, Err(ImageError::Corrupt(_))));
    }

    #[test]
    fn fail_on_error_refuses_what_truncated_does() {
        let png = encoded(&gradient(64, 64), Codec::Png);
        let cut = &png[..png.len() / 2];
        for level in ["error", "warning", "truncated"] {
            let u = json(serde_json::json!({"constructorOptions": {"failOn": level}}));
            assert_eq!(run(cut, &u), Err(ImageError::Truncated), "{level}");
        }
    }

    #[test]
    fn animated_output_is_held_to_the_cap() {
        let u = json(serde_json::json!({
            "resizeOptions": {"width": 100},
            "constructorOptions": {"limitInputPixels": 50_000}
        }));
        // Ten 10×10 frames come in at 1,000 pixels; ten 100×100 frames would go out at 100,000.
        let got = run(&moving_gif(10, 10, 10), &u);
        assert!(got.is_err(), "{:?}", got.map(|p| (p.width, p.height)));
    }

    #[test]
    fn an_icon_repeating_one_png_is_refused() {
        let png = encoded(&crate::fixtures::rgba(4, 4), Codec::Png);
        let ico = ico_repeating(&png, 300);
        let d = dir();
        assert!(probe(&file(&d, &ico), &ico).is_err());
    }

    #[test]
    fn unresolvable_avif_metadata_isnt_stored() {
        let avif = avif_with_exif(20, 12, exif(None, true));
        let extents = crate::fixtures::avif_extents(&avif);
        let at = extents
            .iter()
            .position(|e| avif.get(e.1 + 4..e.1 + 8) == Some(b"MM\0\x2a"))
            .expect("the EXIF item");
        let lost = avif_set_extent(avif, at, 1 << 30, 10);
        let got = run(&lost, &json(serde_json::json!({})));
        if let Ok(out) = got {
            assert!(!has(&stored(&out), b"MM\0\x2a"), "the EXIF was stored");
        }
    }

    #[test]
    fn an_avif_item_with_two_frames_is_refused() {
        let avif = avif_two_frames(16, 16);
        let d = dir();
        let got = probe(&file(&d, &avif), &avif);
        assert!(got.is_err(), "{got:?}");
    }

    #[test]
    fn a_clean_aperture_crops() {
        let avif = avif_cropped(20, 12, 10, 6);
        let out = run(&avif, &json(serde_json::json!({}))).unwrap();
        assert_eq!((out.width, out.height), (10, 6));
        let u = json(serde_json::json!({"formatOptions": {"format": "png"}}));
        let out = run(&avif, &u).unwrap();
        let made = ::image::load_from_memory(&stored(&out)).unwrap();
        assert_eq!((made.width(), made.height()), (10, 6));
    }

    #[test]
    fn an_avif_reencode_keeps_the_profile() {
        let png = png_icc_turned(8, 4, 300);
        let u = json(serde_json::json!({"formatOptions": {"format": "avif"}}));
        let out = run(&png, &u).unwrap();
        let kept = stored(&out);
        let m = avif::meta(&kept).unwrap();
        let (s, e) = m.icc.expect("a colr prof");
        assert_eq!(e - s, 300);
        // It still decodes.
        let d = dir();
        let p = probe(&file(&d, &kept), &kept).unwrap();
        assert_eq!((p.width, p.height), (4, 8));
    }

    #[test]
    fn animations_carry_their_metadata() {
        let xmp = b"<x:xmpmeta>where</x:xmpmeta>";
        let icc = crate::fixtures::fake_icc(600);
        let source = gif::with_meta(&moving_gif(4, 4, 3), Some(&icc), Some(xmp));
        // Without withMetadata the profile stays and the XMP goes.
        let out = run(&source, &json(serde_json::json!({}))).unwrap();
        let walked = gif::walk(&stored(&out), false).unwrap();
        assert_eq!(
            (walked.icc.as_deref(), walked.xmp.as_deref()),
            (Some(&icc[..]), None)
        );
        let u = json(serde_json::json!({
            "withMetadata": true,
            "resizeOptions": {"width": 2},
            "formatOptions": {"format": "webp"}
        }));
        let out = run(&source, &u).unwrap();
        let webp = img_parts::webp::WebP::from_bytes(stored(&out).into()).unwrap();
        assert_eq!(webp_info(&stored(&out)).frames, 3);
        assert_eq!(webp.icc_profile().as_deref(), Some(&icc[..]));
        assert!(webp.chunk_by_id(*b"XMP ").is_some());
        let as_gif = json(serde_json::json!({"withMetadata": true, "resizeOptions": {"width": 2}}));
        let out = run(&source, &as_gif).unwrap();
        let walked = gif::walk(&stored(&out), false).unwrap();
        assert_eq!(walked.frames, 3);
        assert_eq!(walked.xmp.as_deref(), Some(&xmp[..]));
        assert_eq!(walked.icc.as_deref(), Some(&icc[..]));
    }

    #[test]
    fn an_icon_keeps_every_entry() {
        let small = encoded(&crate::fixtures::rgba(4, 4), Codec::Png);
        let big = encoded(&crate::fixtures::rgba(8, 8), Codec::Png);
        let mut p = img_parts::png::Png::from_bytes(big.into()).unwrap();
        let at = p.chunks().len() - 1;
        p.chunks_mut().insert(
            at,
            img_parts::png::PngChunk::new(*b"tEXt", Bytes::from_static(b"GPS\0secret")),
        );
        let ico = ico_of(&[small, p.encoder().bytes().to_vec()]);
        let out = run(&ico, &json(serde_json::json!({}))).unwrap();
        let kept = stored(&out);
        assert!(!has(&kept, b"secret"));
        assert_eq!(u16::from_le_bytes([kept[4], kept[5]]), 2, "both entries");
        assert_eq!((out.width, out.height), (8, 8));
        let d = dir();
        assert!(probe(&file(&d, &kept), &kept).is_ok());
    }

    /// An icon whose one entry declares only the PNG's signature and IHDR (33 bytes), with
    /// the rest of the PNG, a 32 MiB iCCP bomb first, straight after it.
    fn short_entry_icon() -> Vec<u8> {
        let mut ico = ico_around(&iccp_bomb_png(32 << 20));
        ico[14..18].copy_from_slice(&33u32.to_le_bytes());
        ico
    }

    #[test]
    fn an_icon_entry_is_read_within_its_declared_size() {
        let ico = short_entry_icon();
        let d = dir();
        let got = probe(&file(&d, &ico), &ico);
        assert!(got.is_err(), "{got:?}");
        assert!(run(&ico, &json(serde_json::json!({}))).is_err());
    }

    #[test]
    fn overlapping_icon_entries_are_refused() {
        let png = encoded(&crate::fixtures::rgba(4, 4), Codec::Png);
        let mut ico = ico_of(&[png.clone(), png]);
        // The second entry starts where the first does and runs 4 bytes past it.
        let first = u32::from_le_bytes([ico[18], ico[19], ico[20], ico[21]]);
        let size = u32::from_le_bytes([ico[14], ico[15], ico[16], ico[17]]);
        ico[30..34].copy_from_slice(&(size + 4).to_le_bytes());
        ico[34..38].copy_from_slice(&first.to_le_bytes());
        let d = dir();
        assert!(probe(&file(&d, &ico), &ico).is_err());
    }

    #[test]
    fn a_bitmap_icon_decodes_from_its_own_bytes() {
        let ico = crate::fixtures::bitmap_icon();
        let d = dir();
        let input = file(&d, &ico);
        let p = probe(&input, &ico).unwrap();
        assert_eq!((p.width, p.height), (2, 2));
        let decoded = decode(&ico, &p, 100, false, false).unwrap();
        let img = decoded.frames[0].image.to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 0], "blue, masked out");
        assert_eq!(img.get_pixel(1, 0).0, [9, 9, 9, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0, 255]);
    }

    #[test]
    fn a_panic_while_decoding_is_a_corrupt_file() {
        let found = crate::fixtures::avif_that_panics();
        assert!(
            found.is_some() || !cfg!(debug_assertions),
            "no panicking input found"
        );
        let Some(avif) = found else {
            return; // a release build: avif-parse's assertion is compiled out
        };
        let d = dir();
        let input = file(&d, &avif);
        let got = probe(&input, &avif);
        assert!(matches!(got, Err(ImageError::Corrupt(_))), "{got:?}");
        let fake = Probe {
            width: 8,
            height: 8,
            frames: 1,
            orientation: 1,
            format: InputFormat::Avif,
            pixels: 64,
            strip_by_encoding: false,
        };
        let got = process_original(&input, &fake, &json(serde_json::json!({})), 1000, &d);
        assert!(matches!(got, Err(ImageError::Corrupt(_))), "{got:?}");
    }

    #[test]
    fn ravifs_layout_takes_a_profile() {
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let with = avif::with_icc(&avif, &crate::fixtures::fake_icc(100))
            .expect("ravif's container layout changed: avif::with_icc needs updating");
        assert_eq!(
            avif::meta(&with).unwrap().icc.map(|(s, e)| e - s),
            Some(100)
        );
        let d = dir();
        assert!(probe(&file(&d, &with), &with).is_ok());
    }

    #[test]
    fn gif_strip_keeps_only_what_decoding_needs() {
        let mut g = moving_gif(2, 2, 2);
        let mut extra = b"\x21\x01\x0Cplain text!!\x00".to_vec(); // plain text
        extra.extend_from_slice(b"\x21\x99\x02zz\x00"); // unknown label
        extra.extend_from_slice(b"\x21\xFF\x0BNETSCAPE2.0\x05\x01\x00\x00hi\x00"); // padded loop
        extra.extend_from_slice(b"\x21\xFF\x0BICCRGBG1012\x04junk\x00"); // not a profile
        let table = if g[10] & 0x80 != 0 {
            3 << ((g[10] & 7) + 1)
        } else {
            0
        };
        g.splice(13 + table..13 + table, extra);
        let walked = gif::walk(&g, true).unwrap();
        let out = &walked.stripped;
        for gone in [&b"plain"[..], b"zz", b"hi", b"junk"] {
            assert!(!has(out, gone), "{:?} kept", String::from_utf8_lossy(gone));
        }
        assert_eq!(walked.icc, None);
        assert_eq!(gif::walk(out, false).unwrap().frames, 2);
    }

    #[test]
    fn gif_strip_keeps_one_graphic_control_block_per_image() {
        let mut g = moving_gif(2, 2, 2);
        // Three control blocks before the first image, reserved bits set, each with 4
        // bytes that would otherwise ride along.
        let mut extra = Vec::new();
        for payload in [b"AAAA", b"BBBB", b"CCCC"] {
            extra.extend_from_slice(&[0x21, 0xF9, 4]);
            extra.extend_from_slice(payload);
            extra.push(0);
        }
        let table = if g[10] & 0x80 != 0 {
            3 << ((g[10] & 7) + 1)
        } else {
            0
        };
        g.splice(13 + table..13 + table, extra);
        let walked = gif::walk(&g, true).unwrap();
        let out = &walked.stripped;
        let controls: Vec<&[u8]> = out
            .windows(8)
            .filter(|w| w[..3] == [0x21, 0xF9, 4])
            .collect();
        assert_eq!(controls.len(), 2, "one per image");
        assert!(
            controls.iter().all(|c| c[3] & 0xE0 == 0),
            "reserved bits cleared"
        );
        assert!(!has(out, b"AAAA") && !has(out, b"BBBB"));
        assert_eq!(gif::walk(out, false).unwrap().frames, 2);
    }

    #[test]
    fn gif_xmp_needs_its_exact_trailer() {
        let g = gif::with_meta(&moving_gif(2, 2, 2), None, Some(b"<x/>"));
        assert_eq!(
            gif::walk(&g, false).unwrap().xmp.as_deref(),
            Some(&b"<x/>"[..])
        );
        // A packet whose bytes would end the sub-block walk early isn't written.
        let steering = [4u8, b'a', b'b', b'c', b'd', 0, 0x2C, 1, 2];
        let g = gif::with_meta(&moving_gif(2, 2, 2), None, Some(&steering));
        assert_eq!(gif::walk(&g, false).unwrap().xmp, None);
        assert_eq!(gif::walk(&g, false).unwrap().frames, 2);
    }
}
