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

/// The PNG chunks kept when metadata is stripped: what decoding and colour need. Not
/// `sPLT` or `hIST`: a suggested palette and a histogram show nothing.
const PNG_KEEP: &[&[u8; 4]] = &[
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB",
    b"cICP", b"mDCv", b"cLLi", b"bKGD", b"pHYs", b"acTL", b"fcTL", b"fdAT",
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
    /// Fine to read, but past what the format it's made into holds (`made`, with its
    /// article: "a WebP").
    #[error("this image can't be made into {made}: it's {side} pixels across; the limit is {most}")]
    TooWide {
        made: &'static str,
        side: u32,
        most: u32,
    },
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
            let mut bytes = read()?;
            // Read as it will be kept: only its own boxes and items (`process_original`
            // keeps the same, or refuses a file whose end can't be known).
            if let Some(kept) = avif::keep(&bytes, true, 0)? {
                bytes = kept;
            }
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
    /// A TIFF's own descriptive tags, carried into the TIFF it's re-encoded as.
    tiff_tags: Vec<TiffTag>,
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
                if p.format == InputFormat::Tiff {
                    meta.tiff_tags = tiff_tags(bytes);
                }
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
    meta.icc = meta.icc.filter(|icc| icc_shaped(icc));
    Ok(Decoded {
        frames,
        loops,
        meta,
    })
}

/// Whether `icc` is shaped like an ICC profile: the size it declares is its length, and
/// `acsp` is at 36. A profile may carry private tags, so this is all that can be checked;
/// one that isn't so shaped could be anything and isn't kept.
pub(super) fn icc_shaped(icc: &[u8]) -> bool {
    icc.get(36..40) == Some(b"acsp")
        && icc[..4] == u32::try_from(icc.len()).map_or([0; 4], u32::to_be_bytes)
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
    let mut bytes = std::fs::read(input).map_err(internal)?;
    let walked_jpeg = match probe.format {
        InputFormat::Jpeg => Some(jpeg::walk(&bytes)?),
        _ => None,
    };
    let walked_gif = match probe.format {
        InputFormat::Gif => Some(gif::walk(&bytes, !u.with_metadata)?),
        _ => None,
    };
    // What follows the format's own end (a ZIP, a page, a vendor trailer) isn't metadata
    // and isn't the picture: it goes, whatever `withMetadata` keeps, and so does what an
    // icon or an AVIF holds between its parts. An end that can't be found, or that lies,
    // is a file cut off: refused when strict, else re-encoded from what decodes, so no
    // tail survives either way. A TIFF kept with its metadata is cut after the last byte
    // any of its IFDs reach; one laid out in a way that walk doesn't take is re-encoded.
    let uploaded = bytes.len();
    let mut odd_tiff = false;
    let own = match probe.format {
        InputFormat::Jpeg => Own::cut(walked_jpeg.as_ref().and_then(|j| j.ends.then_some(j.end))),
        InputFormat::Gif => Own::cut(walked_gif.as_ref().and_then(|g| g.ends.then_some(g.end))),
        InputFormat::Png => Own::cut(png_end(&bytes)),
        InputFormat::Webp => match webp_end(&bytes)? {
            Some((end, false)) => Own::Cut(end),
            // Given the pad byte its last chunk left out, which other readers expect.
            Some((end, true)) => {
                let mut padded = bytes[..end].to_vec();
                padded.push(0);
                let len = u32::try_from(padded.len() - 8).map_err(|_| ImageError::Truncated)?;
                padded[4..8].copy_from_slice(&len.to_le_bytes());
                Own::Replace(padded)
            }
            None => Own::Unended,
        },
        InputFormat::Bmp => Own::replace(bmp_end(&bytes).map(|end| bmp_profiled(&bytes[..end]))),
        InputFormat::Ico => Own::replace(ico::repack(&bytes, &ico::walk(&bytes)?, u.with_metadata)),
        // Thumbnails get what the cap leaves after the picture, all frames counted.
        InputFormat::Avif => Own::replace(avif::keep(
            &bytes,
            u.with_metadata,
            cap.saturating_sub(probe.pixels),
        )?),
        // Only the bytes its IFDs reach, each where it was; gaps zeroed.
        InputFormat::Tiff if u.with_metadata => match tiff_regions(&bytes) {
            Ok(regions) => Own::Replace(zeroed_but(&bytes, &regions)),
            Err(TiffEnd::Odd) => {
                odd_tiff = true;
                Own::Cut(uploaded)
            }
            Err(TiffEnd::Lies) => Own::Unended,
        },
        InputFormat::Tiff => Own::Cut(uploaded),
    };
    let unended = matches!(own, Own::Unended);
    if strict && unended {
        return Err(ImageError::Truncated);
    }
    let changed = match own {
        Own::Cut(end) => {
            bytes.truncate(end);
            end < uploaded
        }
        Own::Replace(kept) => {
            let changed = kept != bytes;
            bytes = kept;
            changed
        }
        Own::Unended => false,
    };
    let transforms =
        u.trim_options.is_some() || u.resize_options.is_some() || u.format_options.is_some();
    let upright = !u.with_metadata
        && probe.orientation != 1
        && matches!(
            probe.format,
            InputFormat::Jpeg | InputFormat::Png | InputFormat::Webp | InputFormat::Tiff
        );
    let must_encode = (!u.with_metadata && probe.strip_by_encoding) || odd_tiff || unended;
    let encode = transforms || upright || must_encode;
    let all = encode && probe.frames > 1;
    let decoded = match decode(&bytes, probe, cap, all, u.with_metadata) {
        Ok(d) => Some(d),
        Err(e @ (ImageError::TooManyPixels { .. } | ImageError::OverMemory)) => return Err(e),
        Err(e) if strict => return Err(e),
        // `failOn: none`: stored without what a decode would give.
        // What was cut to its end and no longer decodes was cut inside the picture: the
        // end lied, and storing it would store a stub.
        Err(_) if unended || changed => return Err(ImageError::Truncated),
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
            // Repacked above, without metadata.
            InputFormat::Ico => None,
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
        None if changed => write_new(out_dir, &bytes)?,
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

/// The original `input`, as `probe` read it, cut to `rect` (`x, y, width, height` in whole
/// pixels of the picture as shown: upright, as `Processed` describes it) on every frame,
/// and re-encoded in its own format. It goes through the decode `process_original` uses,
/// held to the same cap, and keeps what that keeps: the ICC profile, and with
/// `withMetadata` EXIF (orientation reset to 1) and XMP. `trimOptions`, `resizeOptions`
/// and `formatOptions` aren't applied again: the stored original already had them.
pub fn crop(
    input: &Path,
    probe: &Probe,
    rect: (u32, u32, u32, u32),
    u: &Upload,
    max_pixels: u64,
    out_dir: &Path,
) -> Result<Processed, ImageError> {
    let _working = Working::start();
    unpanicked(|| crop_unguarded(input, probe, rect, u, max_pixels, out_dir))
}

fn crop_unguarded(
    input: &Path,
    probe: &Probe,
    (x, y, w, h): (u32, u32, u32, u32),
    u: &Upload,
    max_pixels: u64,
    out_dir: &Path,
) -> Result<Processed, ImageError> {
    let cap = cap(u, max_pixels);
    check_cap(probe, cap)?;
    let bytes = std::fs::read(input).map_err(internal)?;
    let mut decoded = decode(&bytes, probe, cap, probe.frames > 1, u.with_metadata)?;
    let first = &decoded.frames[0].image;
    let fits = |at: u32, len: u32, whole: u32| {
        len > 0 && u64::from(at) + u64::from(len) <= u64::from(whole)
    };
    if !fits(x, w, first.width()) || !fits(y, h, first.height()) {
        return Err(ImageError::Corrupt(format!(
            "a {w}×{h} crop at {x}, {y} doesn't fit a {}×{} image",
            first.width(),
            first.height()
        )));
    }
    for f in &mut decoded.frames {
        f.image = f.image.crop_imm(x, y, w, h);
    }
    let again = Upload {
        trim_options: None,
        resize_options: None,
        format_options: None,
        ..u.clone()
    };
    transform_and_encode(decoded, &bytes, probe, &again, max_pixels, out_dir)
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
        tiff_tags: if u.with_metadata {
            meta.tiff_tags
        } else {
            Vec::new()
        },
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

/// `bytes` up to the furthest of `regions`, every byte outside them zeroed.
fn zeroed_but(bytes: &[u8], regions: &[(usize, usize)]) -> Vec<u8> {
    let end = regions.iter().map(|r| r.1).max().unwrap_or(0);
    let mut out = vec![0u8; end];
    for &(s, e) in regions {
        out[s..e].copy_from_slice(&bytes[s..e]);
    }
    out
}

/// What of an original is its own: all of it up to an end, a rebuilt copy, or nothing
/// known (no end, or one that lies).
enum Own {
    Cut(usize),
    Replace(Vec<u8>),
    Unended,
}

impl Own {
    fn cut(end: Option<usize>) -> Own {
        end.map_or(Own::Unended, Own::Cut)
    }

    fn replace(kept: Option<Vec<u8>>) -> Own {
        kept.map_or(Own::Unended, Own::Replace)
    }
}

/// Why a TIFF has no end to cut at: laid out in a way the walk doesn't take (BigTIFF, a
/// type it doesn't know), or lying (an offset past the file, a loop).
#[derive(Debug, PartialEq, Eq)]
enum TiffEnd {
    Odd,
    Lies,
}

/// The most IFDs and entries walked in a TIFF, all chains and sub-IFDs counted.
const TIFF_MAX_IFDS: usize = 64;
const TIFF_MAX_ENTRIES: usize = 4096;

/// The byte ranges a classic TIFF is made of, from its header: the header, the IFD chain
/// and its SubIFD, Exif, GPS and Interop IFDs, every value stored out of line, and the
/// strips, tiles, JPEG stream and free blocks the offset/byte-count pairs give.
fn tiff_regions(bytes: &[u8]) -> Result<Vec<(usize, usize)>, TiffEnd> {
    use TiffEnd::{Lies, Odd};
    let big = match bytes.get(..4).ok_or(Lies)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        b"MM\0+" | b"II+\0" => return Err(Odd),
        _ => return Err(Lies),
    };
    let len = bytes.len();
    let u16_at = |at: usize| read_u16(bytes, at, big).ok_or(Lies);
    let u32_at = |at: usize| read_u32(bytes, at, big).ok_or(Lies);
    let fits = |start: usize, n: usize| match start.checked_add(n) {
        Some(e) if e <= len => Ok((start, e)),
        _ => Err(Lies),
    };
    let mut regions = vec![(0, 8)];
    let mut todo = vec![u32_at(4)? as usize];
    let (mut seen, mut entries) = (Vec::new(), 0usize);
    while let Some(ifd) = todo.pop() {
        if ifd == 0 {
            continue;
        }
        if seen.contains(&ifd) {
            return Err(Lies);
        }
        seen.push(ifd);
        if seen.len() > TIFF_MAX_IFDS {
            return Err(Lies);
        }
        let n = usize::from(u16_at(ifd)?);
        entries += n;
        if entries > TIFF_MAX_ENTRIES {
            return Err(Lies);
        }
        regions.push(fits(ifd, 2 + 12 * n + 4)?);
        todo.push(u32_at(ifd + 2 + 12 * n)? as usize);
        // Each entry's values: (tag, the values as numbers when they're offsets or sizes).
        let mut values: Vec<(u16, Vec<usize>)> = Vec::new();
        for e in 0..n {
            let at = ifd + 2 + 12 * e;
            let (tag, kind, count) = (u16_at(at)?, u16_at(at + 2)?, u32_at(at + 4)? as usize);
            let size: usize = match kind {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => return Err(Odd),
            };
            let total = size.checked_mul(count).ok_or(Lies)?;
            let from = if total > 4 {
                let from = u32_at(at + 8)? as usize;
                regions.push(fits(from, total)?);
                from
            } else {
                at + 8
            };
            if matches!(
                tag,
                273 | 279 | 288 | 289 | 324 | 325 | 330 | 513 | 514 | 34665 | 34853 | 40965
            ) {
                let number = |i: usize| match kind {
                    3 => u16_at(from + 2 * i).map(usize::from),
                    4 | 13 => u32_at(from + 4 * i).map(|v| v as usize),
                    _ => Err(Odd),
                };
                values.push((tag, (0..count).map(number).collect::<Result<_, _>>()?));
            }
        }
        let get = |tag: u16| values.iter().find(|(t, _)| *t == tag).map(|(_, v)| v);
        for &tag in &[330, 34665, 34853, 40965] {
            todo.extend(get(tag).into_iter().flatten());
        }
        for (offsets, counts) in [(273, 279), (324, 325), (288, 289), (513, 514)] {
            match (get(offsets), get(counts)) {
                (Some(o), Some(c)) if o.len() == c.len() => {
                    for (&o, &c) in o.iter().zip(c) {
                        regions.push(fits(o, c)?);
                    }
                }
                (None, None) => {}
                _ => return Err(Lies),
            }
        }
    }
    Ok(regions)
}

/// Where a PNG ends: just past an `IEND` chunk of length 0, every chunk before it whole.
/// `None` when there's no such end.
fn png_end(bytes: &[u8]) -> Option<usize> {
    let mut i = 8;
    while let Some(h) = bytes.get(i..i + 8) {
        let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let next = i.checked_add(12)?.checked_add(len)?;
        if next > bytes.len() {
            return None;
        }
        if &h[4..] == b"IEND" {
            return (len == 0).then_some(next);
        }
        i = next;
    }
    None
}

/// Where a WebP ends, as its RIFF header says, and whether its last chunk left out its pad
/// byte: `None` when that's past the file's end, or when the chunks inside don't fill it
/// exactly (bytes that are no whole chunk). A length too short to hold even `WEBP` is no
/// WebP.
fn webp_end(bytes: &[u8]) -> Result<Option<(usize, bool)>, ImageError> {
    let len = bytes
        .get(4..8)
        .map(|l| u32::from_le_bytes([l[0], l[1], l[2], l[3]]) as usize)
        .ok_or(ImageError::Truncated)?;
    if len < 4 {
        return Err(ImageError::Corrupt(format!(
            "its RIFF length is {len}, too short for a WebP"
        )));
    }
    let end = len + 8;
    if end > bytes.len() {
        return Ok(None);
    }
    let mut i = 12;
    let mut unpadded = false;
    while i < end {
        let Some(h) = bytes.get(i..i + 8).filter(|_| i + 8 <= end) else {
            return Ok(None);
        };
        let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        i = match (i + 8).checked_add(size + (size & 1)) {
            Some(next) if next <= end => next,
            // The last chunk's pad byte left out, as some muxers write it.
            Some(next) if next == end + 1 && size & 1 == 1 => {
                unpadded = true;
                end
            }
            _ => return Ok(None),
        };
    }
    Ok(Some((end, unpadded)))
}

/// Where a BMP ends, as its file header's size says, or when that's 0 (some writers leave
/// it so) where its pixels do: their offset plus the image size, or rows of the image's
/// own stride when that's 0 too. `None` when the end is shorter than the headers or past
/// the file's end.
fn bmp_end(bytes: &[u8]) -> Option<usize> {
    let u32_at = |i: usize| {
        let b = bytes.get(i..i + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let end = match u32_at(2)? {
        0 => {
            let pixels = match u32_at(34)? {
                0 => {
                    let w = (u32_at(18)? as i32).unsigned_abs() as usize;
                    let h = (u32_at(22)? as i32).unsigned_abs() as usize;
                    let bits = usize::from(u16::from_le_bytes(bytes.get(28..30)?.try_into().ok()?));
                    w.checked_mul(bits)?
                        .div_ceil(32)
                        .checked_mul(4)?
                        .checked_mul(h)?
                }
                n => n,
            };
            u32_at(10)?.checked_add(pixels)?
        }
        n => n,
    };
    (54..=bytes.len()).contains(&end).then_some(end)
}

/// `bmp` with the profile a version 5 header embeds (`bV5CSType` `PROFILE_EMBEDDED`)
/// zeroed when it isn't shaped like one; any other BMP as it is.
fn bmp_profiled(bmp: &[u8]) -> Vec<u8> {
    let mut out = bmp.to_vec();
    let u32_at = |i: usize| {
        bmp.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    // The header starts at 14; its profile's offset is counted from there.
    if u32_at(14) != Some(124) || bmp.get(70..74) != Some(b"DEBM") {
        return out;
    }
    let (Some(at), Some(len)) = (u32_at(14 + 112), u32_at(14 + 116)) else {
        return out;
    };
    let from = (14 + at).min(bmp.len());
    let to = from.saturating_add(len).min(bmp.len());
    if !icc_shaped(&bmp[from..to]) {
        out[from..to].fill(0);
    }
    out
}

/// A TIFF tag kept as bytes: its number, type, count and value. Only byte, text and
/// undefined types, whose values read the same in either byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TiffTag {
    tag: u16,
    kind: u16,
    count: u32,
    value: Vec<u8>,
}

/// What a TIFF says about itself, carried when it's re-encoded with `withMetadata`: its
/// description, camera, software, date, artist, host, copyright, XMP and IPTC.
const TIFF_KEEP: [u16; 10] = [270, 271, 272, 305, 306, 315, 316, 700, 33432, 33723];

/// A TIFF's byte order (`true` for big-endian) and first IFD, read with `get` only.
fn tiff_ifd0(bytes: &[u8]) -> Option<(bool, usize)> {
    let big = match bytes.get(..4)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        _ => return None,
    };
    Some((big, read_u32(bytes, 4, big)? as usize))
}

fn read_u16(b: &[u8], at: usize, big: bool) -> Option<u16> {
    let x: [u8; 2] = b.get(at..at + 2)?.try_into().ok()?;
    Some(if big {
        u16::from_be_bytes(x)
    } else {
        u16::from_le_bytes(x)
    })
}

fn read_u32(b: &[u8], at: usize, big: bool) -> Option<u32> {
    let x: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(if big {
        u32::from_be_bytes(x)
    } else {
        u32::from_le_bytes(x)
    })
}

/// The `TIFF_KEEP` tags of `bytes`' first IFD that are bytes, text or undefined; any that
/// can't be read are left out.
fn tiff_tags(bytes: &[u8]) -> Vec<TiffTag> {
    let Some((big, ifd)) = tiff_ifd0(bytes) else {
        return Vec::new();
    };
    let n = read_u16(bytes, ifd, big).unwrap_or(0);
    (0..usize::from(n))
        .filter_map(|e| {
            let at = ifd + 2 + 12 * e;
            let tag = read_u16(bytes, at, big)?;
            let kind = read_u16(bytes, at + 2, big)?;
            let count = read_u32(bytes, at + 4, big)?;
            if !TIFF_KEEP.contains(&tag) || ![1, 2, 7].contains(&kind) {
                return None;
            }
            let len = count as usize;
            let value = if len <= 4 {
                bytes.get(at + 8..at + 8 + len)?
            } else {
                let from = read_u32(bytes, at + 8, big)? as usize;
                bytes.get(from..from.checked_add(len)?)?
            };
            Some(TiffTag {
                tag,
                kind,
                count,
                value: value.to_vec(),
            })
        })
        .collect()
}

/// `tiff`, as our encoder wrote it, with `tags` added to its first IFD: a new IFD, sorted
/// by tag, goes at the end with the values that don't fit in an entry, and the header
/// points at it. A tag the encoder wrote itself stays as it wrote it.
fn with_tiff_tags(mut tiff: Vec<u8>, tags: &[TiffTag]) -> Vec<u8> {
    let Some((big, ifd)) = tiff_ifd0(&tiff).filter(|_| !tags.is_empty()) else {
        return tiff;
    };
    let Some(n) = read_u16(&tiff, ifd, big) else {
        return tiff;
    };
    let Some(old) = tiff.get(ifd + 2..ifd + 2 + 12 * usize::from(n)) else {
        return tiff;
    };
    let mut entries: Vec<[u8; 12]> = old.as_chunks::<12>().0.to_vec();
    let tag_of = |e: &[u8; 12]| read_u16(e, 0, big).unwrap_or(0);
    let added: Vec<&TiffTag> = tags
        .iter()
        .filter(|t| !entries.iter().any(|e| tag_of(e) == t.tag))
        .collect();
    let u16b = |v: u16| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let u32b = |v: u32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    // Word-aligned, as IFDs and values are.
    if tiff.len() % 2 == 1 {
        tiff.push(0);
    }
    let count = entries.len() + added.len();
    let new_ifd = tiff.len();
    let mut values_at = new_ifd + 2 + 12 * count + 4;
    let mut values = Vec::new();
    for t in &added {
        let mut e = [0u8; 12];
        e[..2].copy_from_slice(&u16b(t.tag));
        e[2..4].copy_from_slice(&u16b(t.kind));
        e[4..8].copy_from_slice(&u32b(t.count));
        if t.value.len() <= 4 {
            e[8..8 + t.value.len()].copy_from_slice(&t.value);
        } else {
            e[8..].copy_from_slice(&u32b(values_at as u32));
            values.extend_from_slice(&t.value);
            if t.value.len() % 2 == 1 {
                values.push(0);
            }
            values_at += t.value.len() + t.value.len() % 2;
        }
        entries.push(e);
    }
    entries.sort_by_key(tag_of);
    tiff.extend_from_slice(&u16b(count as u16));
    for e in &entries {
        tiff.extend_from_slice(e);
    }
    // Only the first IFD is rewritten; the encoder writes no other.
    tiff.extend_from_slice(&[0; 4]);
    tiff.extend_from_slice(&values);
    let at = u32b(new_ifd as u32);
    tiff[4..8].copy_from_slice(&at);
    tiff
}

/// A new file in `dir` holding `bytes`, and its size. Nothing is left behind on failure.
fn write_new(dir: &Path, bytes: &[u8]) -> Result<(PathBuf, u64), ImageError> {
    let mut file = crate::spool_in(dir).map_err(internal)?;
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
            // The first profile, inflated as the probe allowed, when it's shaped like one.
            let mut icc = png.icc_profile().is_some_and(|i| icc_shaped(&i));
            png.chunks_mut().retain(|c| match &c.kind() {
                b"iCCP" => std::mem::take(&mut icc),
                kind => PNG_KEEP.contains(&kind),
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
            let icc = webp.icc_profile().filter(|i| icc_shaped(i));
            webp.set_icc_profile(icc);
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
    fits(img.width(), img.height(), format)?;
    let mut out = Vec::new();
    match format {
        InputFormat::Jpeg => {
            let (data, colour) = if img.color().has_color() {
                (img.to_rgb8().into_raw(), jpeg_encoder::ColorType::Rgb)
            } else {
                (img.to_luma8().into_raw(), jpeg_encoder::ColorType::Luma)
            };
            // Under 65,536 each, by `fits`.
            let side = |v: u32| u16::try_from(v).map_err(failed);
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
            let speed = avif_speed(o.effort, u64::from(img.width()) * u64::from(img.height()));
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
            out = with_tiff_tags(cursor.into_inner(), &meta.tiff_tags);
        }
        InputFormat::Bmp | InputFormat::Ico => {
            let mut cursor = Cursor::new(Vec::new());
            img.write_to(&mut cursor, codec(format)).map_err(failed)?;
            out = cursor.into_inner();
        }
    }
    Ok(out)
}

/// Refuses (`TooWide`, a `422`) a `w`×`h` wider or taller than `format`'s encoder takes:
/// JPEG, GIF and AVIF 65,535, WebP 16,383, an icon 256.
fn fits(w: u32, h: u32, format: InputFormat) -> Result<(), ImageError> {
    let (made, most) = match format {
        InputFormat::Jpeg => ("a JPEG", 65_535),
        InputFormat::Gif => ("a GIF", 65_535),
        InputFormat::Webp => ("a WebP", 16_383),
        InputFormat::Ico => ("an icon", 256),
        InputFormat::Avif => ("an AVIF", 65_535),
        _ => return Ok(()),
    };
    match w.max(h) {
        side if side > most => Err(ImageError::TooWide { made, side, most }),
        _ => Ok(()),
    }
}

/// ravif's speed for `effort` (4 when unset): 10 less it, but at least 9 above 16 MP,
/// so a picture at the 50 MP cap encodes in about 30 s (61 s at effort 4, release, 12 cores)
/// while it holds a decode permit.
fn avif_speed(effort: Option<u8>, pixels: u64) -> u8 {
    let fastest = if pixels > 16_000_000 { 9 } else { 1 };
    10u8.saturating_sub(effort.unwrap_or(4)).clamp(fastest, 10)
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
    if let Some(f) = frames.first() {
        fits(f.image.width(), f.image.height(), format)?;
    }
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

    /// The four ways an original is kept: strict or `failOn: none`, with or without
    /// `withMetadata`.
    fn settings() -> [(&'static str, Upload); 4] {
        let none = serde_json::json!({"failOn": "none"});
        [
            ("strict", upload(serde_json::json!({}))),
            (
                "strict+meta",
                upload(serde_json::json!({"withMetadata": true})),
            ),
            (
                "lenient",
                upload(serde_json::json!({"constructorOptions": none})),
            ),
            (
                "lenient+meta",
                upload(serde_json::json!({"withMetadata": true, "constructorOptions": none})),
            ),
        ]
    }

    const TAIL: &[u8] = b"<html><script>alert(1)</script></html>PK\x05\x06";

    fn tailed(mut bytes: Vec<u8>) -> Vec<u8> {
        bytes.extend_from_slice(TAIL);
        bytes
    }

    /// `png` with the chunk of type `kind` changed by `edit`.
    fn png_chunk(png: &[u8], kind: &[u8; 4], edit: impl Fn(&mut Vec<u8>, usize)) -> Vec<u8> {
        let mut out = png.to_vec();
        let at = (8..out.len() - 8)
            .find(|&i| &out[i + 4..i + 8] == kind)
            .expect("the chunk");
        edit(&mut out, at);
        out
    }

    fn riff_len(mut webp: Vec<u8>, len: u32) -> Vec<u8> {
        webp[4..8].copy_from_slice(&len.to_le_bytes());
        webp
    }

    #[test]
    fn what_follows_the_end_marker_is_dropped() {
        let tiff = tiff_with_description("kept");
        let ends: [(&str, Vec<u8>, &[u8]); 8] = [
            (
                "png",
                encoded(&gradient(8, 8), Codec::Png),
                b"IEND\xAE\x42\x60\x82",
            ),
            ("jpeg", encoded(&gradient(8, 8), Codec::Jpeg), &[0xFF, 0xD9]),
            ("gif", encoded(&gradient(8, 8), Codec::Gif), &[0x3B]),
            ("webp", encoded(&gradient(8, 8), Codec::WebP), &[]),
            ("bmp", encoded(&gradient(8, 8), Codec::Bmp), &[]),
            (
                "ico",
                ico_around(&encoded(&gradient(8, 8), Codec::Png)),
                &[],
            ),
            ("avif", encoded(&gradient(8, 8), Codec::Avif), &[]),
            ("tiff", tiff, &[]),
        ];
        for (name, clean, end) in ends {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let out = run(&tailed(clean.clone()), &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                let kept = stored(&out);
                assert!(!has(&kept, b"<html>"), "{at}: the tail stayed");
                assert!(!has(&kept, b"PK\x05\x06"), "{at}: the tail stayed");
                assert_eq!(out.filesize, kept.len() as u64, "{at}");
                if name == "tiff" {
                    // Re-encoded whatever is kept, a TIFF having no end to cut at; with
                    // `withMetadata` its description comes along.
                    assert_eq!(has(&kept, b"kept"), u.with_metadata, "{at}");
                    assert_eq!(::image::load_from_memory(&kept).unwrap().width(), 2, "{at}");
                    continue;
                }
                assert!(kept.ends_with(end), "{at}");
                if u.with_metadata {
                    // Nothing else changes: the file is as uploaded, up to its end.
                    assert_eq!(kept, clean, "{at}");
                }
            }
        }

        // Ends that are missing or lie: refused when strict, and without the tail when
        // not; and a RIFF length too short to hold a WebP refused either way.
        let png = encoded(&gradient(8, 8), Codec::Png);
        let jpeg = encoded(&gradient(8, 8), Codec::Jpeg);
        let gif = encoded(&gradient(8, 8), Codec::Gif);
        let webp = encoded(&gradient(8, 8), Codec::WebP);
        let bmp = encoded(&gradient(8, 8), Codec::Bmp);
        let no_iend = tailed(png[..png.len() - 12].to_vec());
        let overrun = {
            let mut p = png[..png.len() - 12].to_vec();
            p.extend_from_slice(&1_000u32.to_be_bytes());
            p.extend_from_slice(b"zzZz");
            tailed(p)
        };
        let iend_covers = png_chunk(&tailed(png.clone()), b"IEND", |p, at| {
            p[at..at + 4].copy_from_slice(&(TAIL.len() as u32).to_be_bytes());
        });
        let covers = |extra: usize| {
            let mut w = tailed(webp.clone());
            w.extend(std::iter::repeat_n(b'x', extra));
            let len = (w.len() - 8) as u32;
            riff_len(w, len)
        };
        let bmp_short = {
            let mut b = tailed(bmp.clone());
            b[2..6].copy_from_slice(&10u32.to_le_bytes());
            b
        };
        let bmp_into_pixels = {
            let mut b = tailed(bmp.clone());
            b[2..6].copy_from_slice(&60u32.to_le_bytes());
            b
        };
        let small = encoded(&gradient(4, 4), Codec::Png);
        let two_entries = ico_of(&[small[..small.len() - 12].to_vec(), png.clone()]);
        let lying: [(&str, Vec<u8>); 13] = [
            ("ico with a smaller entry without IEND", two_entries),
            (
                "ico PNG entry without IEND",
                ico_around(&tailed(png[..png.len() - 12].to_vec())),
            ),
            ("png without IEND", no_iend),
            ("png chunk past the end", overrun),
            ("png IEND covering the tail", iend_covers),
            ("webp RIFF covering the tail, even", covers(0)),
            ("webp RIFF covering the tail, odd", covers(1)),
            (
                "webp RIFF 0xFFFFFFFF",
                riff_len(tailed(webp.clone()), u32::MAX),
            ),
            ("jpeg without EOI", tailed(jpeg[..jpeg.len() - 2].to_vec())),
            ("gif without trailer", tailed(gif[..gif.len() - 1].to_vec())),
            ("bmp size too small", bmp_short),
            ("bmp size inside the pixels", bmp_into_pixels),
            ("webp RIFF length 4", riff_len(tailed(webp.clone()), 4)),
        ];
        for (name, bytes) in lying {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                match run(&bytes, &u) {
                    Ok(out) => {
                        assert!(how.starts_with("lenient"), "{at}: stored when strict");
                        let kept = stored(&out);
                        assert!(!has(&kept, b"<html>"), "{at}: the tail stayed");
                        assert_eq!(out.filesize, kept.len() as u64, "{at}");
                        // Re-encoded whole, never a stub cut short of the picture: it goes
                        // through again, strictly, decoding to its full size.
                        let again = run(&kept, &upload(serde_json::json!({})))
                            .unwrap_or_else(|e| panic!("{at}: stored but unreadable: {e}"));
                        assert_eq!((again.width, again.height), (8, 8), "{at}");
                    }
                    Err(e) => assert!(
                        matches!(e, ImageError::Truncated | ImageError::Corrupt(_)),
                        "{at}: {e}"
                    ),
                }
            }
        }
        for len in [0, 3] {
            for (how, u) in settings() {
                let res = run(&riff_len(webp.clone(), len), &u);
                assert!(res.is_err(), "RIFF length {len}, {how}: stored");
            }
        }
    }

    /// The top-level boxes of an ISOBMFF file: (start, size field's value, end).
    fn top_boxes(data: &[u8]) -> Vec<(usize, u32, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap());
            let end = match size {
                0 => data.len(),
                1 => i + u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()) as usize,
                n => i + n as usize,
            };
            out.push((i, size, end));
            i = end;
        }
        out
    }

    /// A box of `kind` holding `body`, with a 64-bit size when `large`.
    fn isobmff_box(kind: &[u8; 4], body: &[u8], large: bool) -> Vec<u8> {
        let mut b = Vec::new();
        if large {
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(kind);
            b.extend_from_slice(&(16 + body.len() as u64).to_be_bytes());
        } else {
            b.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
            b.extend_from_slice(kind);
        }
        b.extend_from_slice(body);
        b
    }

    /// A value of a TIFF entry: in the entry, or placed after the IFD.
    enum TiffValue {
        In(u32),
        Out(Vec<u8>),
    }

    /// Writes an IFD of `entries` at the end of `out`, in `big`-endian order or little, its
    /// values after it, pointing on to `next`; returns where it starts.
    fn put_ifd(
        out: &mut Vec<u8>,
        big: bool,
        entries: Vec<(u16, u16, u32, TiffValue)>,
        next: u32,
    ) -> u32 {
        let u16b = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        if out.len() % 2 == 1 {
            out.push(0);
        }
        let at = out.len();
        let mut values_at = at + 2 + 12 * entries.len() + 4;
        let mut values = Vec::new();
        out.extend_from_slice(&u16b(entries.len() as u16));
        for (tag, kind, count, v) in entries {
            out.extend_from_slice(&u16b(tag));
            out.extend_from_slice(&u16b(kind));
            out.extend_from_slice(&u32b(count));
            match v {
                // A short sits at the start of the field; bytes are in file order.
                TiffValue::In(x) if kind == 3 => {
                    out.extend_from_slice(&u16b(x as u16));
                    out.extend_from_slice(&[0, 0]);
                }
                TiffValue::In(x) if matches!(kind, 1 | 2 | 7) => {
                    out.extend_from_slice(&x.to_le_bytes());
                }
                TiffValue::In(x) => out.extend_from_slice(&u32b(x)),
                TiffValue::Out(b) => {
                    out.extend_from_slice(&u32b(values_at as u32));
                    values_at += b.len() + b.len() % 2;
                    values.extend_from_slice(&b);
                    if b.len() % 2 == 1 {
                        values.push(0);
                    }
                }
            }
        }
        out.extend_from_slice(&u32b(next));
        out.extend_from_slice(&values);
        at as u32
    }

    /// The entries of a 2×2 grey page whose pixels are at `pixels`.
    fn page(pixels: u32) -> Vec<(u16, u16, u32, TiffValue)> {
        use TiffValue::In;
        vec![
            (256, 3, 1, In(2)),
            (257, 3, 1, In(2)),
            (258, 3, 1, In(8)),
            (259, 3, 1, In(1)),
            (262, 3, 1, In(1)),
            (273, 4, 1, In(pixels)),
            (278, 3, 1, In(2)),
            (279, 4, 1, In(4)),
        ]
    }

    /// A little-endian TIFF with an Exif IFD and a GPS IFD (each with a value out of line),
    /// or of two pages, made by hand.
    fn tiff_with(gps: bool, pages: usize) -> Vec<u8> {
        tiff_in(false, gps, pages)
    }

    fn tiff_in(big: bool, gps: bool, pages: usize) -> Vec<u8> {
        use TiffValue::{In, Out};
        let u32b = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut out = if big {
            b"MM\0*\0\0\0\0".to_vec()
        } else {
            b"II*\0\0\0\0\0".to_vec()
        };
        let first = out.len() as u32;
        out.extend_from_slice(&[10, 80, 160, 240]);
        let mut next = 0;
        if pages == 2 {
            let second = out.len() as u32;
            out.extend_from_slice(&[240, 160, 80, 10]);
            next = put_ifd(&mut out, big, page(second), 0);
        }
        let mut ifd0 = page(first);
        if gps {
            // Pointed at once they're written, after IFD0: only the walk into them reaches
            // the end of the file.
            ifd0.push((34665, 4, 1, In(0)));
            ifd0.push((34853, 4, 1, In(0)));
        }
        let at = put_ifd(&mut out, big, ifd0, next);
        out[4..8].copy_from_slice(&u32b(at));
        if gps {
            let exif = put_ifd(
                &mut out,
                big,
                vec![(36867, 2, 20, Out(b"2001:02:03 04:05:06\0".to_vec()))],
                0,
            );
            let rational: Vec<u8> = [1u32, 1, 2, 1, 3, 1]
                .iter()
                .flat_map(|&v| u32b(v))
                .collect();
            let gps = put_ifd(
                &mut out,
                big,
                vec![(1, 2, 2, In(u32::from(b'N'))), (2, 5, 3, Out(rational))],
                0,
            );
            let at = at as usize;
            let n = usize::from(read_u16(&out, at, big).unwrap());
            for e in 0..n {
                let entry = at + 2 + 12 * e;
                let value = match read_u16(&out, entry, big).unwrap() {
                    34665 => exif,
                    34853 => gps,
                    _ => continue,
                };
                out[entry + 8..entry + 12].copy_from_slice(&u32b(value));
            }
        }
        out
    }

    /// `tiff` with its IFD0 copied to the end, past `payload`, and the header pointing at
    /// the copy: the old IFD0 and the payload are reached by nothing.
    fn tiff_moved_ifd0(mut tiff: Vec<u8>, payload: &[u8]) -> Vec<u8> {
        let ifd0 = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
        let n = usize::from(u16::from_le_bytes([tiff[ifd0], tiff[ifd0 + 1]]));
        let copy = tiff[ifd0..ifd0 + 2 + 12 * n + 4].to_vec();
        tiff.extend_from_slice(payload);
        if tiff.len() % 2 == 1 {
            tiff.push(0);
        }
        let at = tiff.len() as u32;
        tiff.extend_from_slice(&copy);
        tiff[4..8].copy_from_slice(&at.to_le_bytes());
        tiff
    }

    /// The pixels `bytes` decodes to, as RGBA.
    fn pixels_of(bytes: &[u8]) -> Vec<u8> {
        let d = dir();
        let input = file(&d, bytes);
        let p = probe(&input, &bytes[..bytes.len().min(8192)]).unwrap();
        decode(bytes, &p, 50_000_000, false, false).unwrap().frames[0]
            .image
            .to_rgba8()
            .into_raw()
    }

    /// The boxes directly inside `data[from..to]` with 32-bit sizes: (start, end, type).
    fn child_boxes(data: &[u8], from: usize, to: usize) -> Vec<(usize, usize, [u8; 4])> {
        let mut out = Vec::new();
        let mut i = from;
        while i + 8 <= to {
            let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            out.push((i, i + size, data[i + 4..i + 8].try_into().unwrap()));
            i += size;
        }
        out
    }

    /// `avif` with `bytes` put at `at`, the boxes starting at `parents` grown to hold them,
    /// and every `iloc` extent at or past `at` moved along.
    fn avif_insert(mut a: Vec<u8>, at: usize, bytes: &[u8], parents: &[usize]) -> Vec<u8> {
        a.splice(at..at, bytes.iter().copied());
        for &p in parents {
            let size = u32::from_be_bytes(a[p..p + 4].try_into().unwrap()) + bytes.len() as u32;
            a[p..p + 4].copy_from_slice(&size.to_be_bytes());
        }
        for (i, (_, off, len)) in crate::fixtures::avif_extents(&a).into_iter().enumerate() {
            if off >= at {
                let moved = (off + bytes.len()) as u32;
                a = crate::fixtures::avif_set_extent(a, i, moved, len as u32);
            }
        }
        a
    }

    /// `avif` with a `free` box holding the tail as the last box inside `meta`.
    fn avif_free_in_meta(avif: &[u8]) -> Vec<u8> {
        let (meta, end, _) = child_boxes(avif, 0, avif.len())
            .into_iter()
            .find(|b| &b.2 == b"meta")
            .unwrap();
        avif_insert(
            avif.to_vec(),
            end,
            &isobmff_box(b"free", TAIL, false),
            &[meta],
        )
    }

    /// `avif` with one more item, item 99: an HTML page, with its own `infe` (`mime`,
    /// `text/html`) and `iloc` entry, its bytes at the end of `mdat`.
    fn avif_extra_item(avif: &[u8]) -> Vec<u8> {
        let find = |a: &[u8], kind: &[u8; 4]| {
            let (meta, end, _) = child_boxes(a, 0, a.len())
                .into_iter()
                .find(|b| &b.2 == b"meta")
                .unwrap();
            let child = child_boxes(a, meta + 12, end)
                .into_iter()
                .find(|b| &b.2 == kind)
                .unwrap();
            (meta, child.0, child.1)
        };
        // The iloc entry: one extent, placed below.
        let (meta, iloc, iloc_end) = find(avif, b"iloc");
        let body = iloc + 8;
        let version = avif[body];
        assert_eq!(
            (avif[body + 4], avif[body + 5]),
            (0x44, 0),
            "4-byte offsets, no base"
        );
        let wide = if version < 2 { 2 } else { 4 };
        let mut entry = 99u32.to_be_bytes()[4 - wide..].to_vec();
        if version >= 1 {
            entry.extend_from_slice(&[0, 0]);
        }
        entry.extend_from_slice(&[0, 0, 0, 1]);
        entry.extend_from_slice(&[0; 8]);
        let mut a = avif_insert(avif.to_vec(), iloc_end, &entry, &[meta, iloc]);
        let count_at = body + 6;
        let count = a[count_at..count_at + wide]
            .iter()
            .fold(0u32, |v, &b| v << 8 | u32::from(b))
            + 1;
        a[count_at..count_at + wide].copy_from_slice(&count.to_be_bytes()[4 - wide..]);
        // Its infe, version 2.
        let (meta, iinf, iinf_end) = find(&a, b"iinf");
        let mut infe = vec![2, 0, 0, 0, 0, 99, 0, 0];
        infe.extend_from_slice(b"mime");
        infe.extend_from_slice(b"page\0text/html\0");
        let a2 = avif_insert(
            a,
            iinf_end,
            &isobmff_box(b"infe", &infe, false),
            &[meta, iinf],
        );
        let mut a = a2;
        let wide = if a[iinf + 8] == 0 { 2 } else { 4 };
        let at = iinf + 12;
        let count = a[at..at + wide]
            .iter()
            .fold(0u32, |v, &b| v << 8 | u32::from(b))
            + 1;
        a[at..at + wide].copy_from_slice(&count.to_be_bytes()[4 - wide..]);
        // Its bytes, at the end of mdat, the last box.
        let (mdat, _, kind) = *child_boxes(&a, 0, a.len()).last().unwrap();
        assert_eq!(&kind, b"mdat");
        let offset = a.len() as u32;
        a.extend_from_slice(TAIL);
        let size = u32::from_be_bytes(a[mdat..mdat + 4].try_into().unwrap()) + TAIL.len() as u32;
        a[mdat..mdat + 4].copy_from_slice(&size.to_be_bytes());
        let index = crate::fixtures::avif_extents(&a)
            .iter()
            .position(|e| e.0 == 99)
            .unwrap();
        crate::fixtures::avif_set_extent(a, index, offset, TAIL.len() as u32)
    }

    /// The `meta` child of `a` of type `kind`: (meta's start, the child's start, its end).
    fn meta_child(a: &[u8], kind: &[u8; 4]) -> Option<(usize, usize, usize)> {
        let (meta, end, _) = child_boxes(a, 0, a.len())
            .into_iter()
            .find(|b| &b.2 == b"meta")?;
        let child = child_boxes(a, meta + 12, end)
            .into_iter()
            .find(|b| &b.2 == kind)?;
        Some((meta, child.0, child.1))
    }

    /// `a` with its `iloc` rewritten as version 1, which can say where an item is
    /// constructed from (4-byte offsets and lengths, no base, no index).
    fn avif_iloc_v1(a: Vec<u8>) -> Vec<u8> {
        let (meta, iloc, end) = meta_child(&a, b"iloc").unwrap();
        let body = iloc + 8;
        if a[body] == 1 {
            return a;
        }
        assert_eq!((a[body], a[body + 4], a[body + 5]), (0, 0x44, 0));
        let u16_at = |i: usize| u16::from_be_bytes([a[i], a[i + 1]]);
        let count = u16_at(body + 6);
        let mut out = vec![1, 0, 0, 0, 0x44, 0];
        out.extend_from_slice(&count.to_be_bytes());
        let mut i = body + 8;
        for _ in 0..count {
            out.extend_from_slice(&a[i..i + 2]);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&a[i + 2..i + 4]);
            let n = usize::from(u16_at(i + 4));
            out.extend_from_slice(&a[i + 4..i + 6 + 8 * n]);
            i += 6 + 8 * n;
        }
        let boxed = isobmff_box(b"iloc", &out, false);
        let grow = boxed.len() - (end - iloc);
        let old: Vec<_> = crate::fixtures::avif_extents(&a);
        let mut b = a.clone();
        b.splice(iloc..end, boxed);
        let size = u32::from_be_bytes(b[meta..meta + 4].try_into().unwrap()) + grow as u32;
        b[meta..meta + 4].copy_from_slice(&size.to_be_bytes());
        for (k, (_, off, len)) in old.into_iter().enumerate() {
            if off >= end {
                b = crate::fixtures::avif_set_extent(b, k, (off + grow) as u32, len as u32);
            }
        }
        b
    }

    /// `a` with one more item: `id`, of type `kind` (with `content`, its content type, for
    /// `mime`), holding `data` in `mdat`, or in `idat` (construction method 1) when
    /// `in_idat`.
    fn avif_with_item(
        a: &[u8],
        id: u16,
        kind: &[u8; 4],
        content: Option<&str>,
        data: &[u8],
        in_idat: bool,
    ) -> Vec<u8> {
        let mut a = if in_idat {
            avif_iloc_v1(a.to_vec())
        } else {
            a.to_vec()
        };
        let (meta, iloc, iloc_end) = meta_child(&a, b"iloc").unwrap();
        let body = iloc + 8;
        let version = a[body];
        let mut entry = id.to_be_bytes().to_vec();
        if version >= 1 {
            entry.extend_from_slice(&[0, u8::from(in_idat)]);
        }
        entry.extend_from_slice(&[0, 0, 0, 1]);
        entry.extend_from_slice(&[0; 8]);
        a = avif_insert(a, iloc_end, &entry, &[meta, iloc]);
        let count = u16::from_be_bytes([a[body + 6], a[body + 7]]) + 1;
        a[body + 6..body + 8].copy_from_slice(&count.to_be_bytes());
        let (meta, iinf, iinf_end) = meta_child(&a, b"iinf").unwrap();
        let mut infe = vec![2, 0, 0, 0];
        infe.extend_from_slice(&id.to_be_bytes());
        infe.extend_from_slice(&[0, 0]);
        infe.extend_from_slice(kind);
        infe.extend_from_slice(b"item\0");
        if let Some(c) = content {
            infe.extend_from_slice(c.as_bytes());
            infe.push(0);
        }
        a = avif_insert(
            a,
            iinf_end,
            &isobmff_box(b"infe", &infe, false),
            &[meta, iinf],
        );
        assert_eq!(a[iinf + 8], 0, "iinf version 0");
        let count = u16::from_be_bytes([a[iinf + 12], a[iinf + 13]]) + 1;
        a[iinf + 12..iinf + 14].copy_from_slice(&count.to_be_bytes());
        let offset = if in_idat {
            let (meta, _, _) = meta_child(&a, b"iinf").unwrap();
            let end = child_boxes(&a, 0, a.len())
                .into_iter()
                .find(|b| &b.2 == b"meta")
                .unwrap()
                .1;
            a = avif_insert(a, end, &isobmff_box(b"idat", data, false), &[meta]);
            0
        } else {
            let (mdat, _, kind) = *child_boxes(&a, 0, a.len()).last().unwrap();
            assert_eq!(&kind, b"mdat");
            let offset = a.len() as u32;
            a.extend_from_slice(data);
            let size =
                u32::from_be_bytes(a[mdat..mdat + 4].try_into().unwrap()) + data.len() as u32;
            a[mdat..mdat + 4].copy_from_slice(&size.to_be_bytes());
            offset
        };
        let index = crate::fixtures::avif_extents(&a)
            .iter()
            .position(|e| e.0 == u32::from(id))
            .unwrap();
        crate::fixtures::avif_set_extent(a, index, offset, data.len() as u32)
    }

    /// `a` with a reference of type `kind` from item `from` to item `to`.
    fn avif_with_ref(a: &[u8], kind: &[u8; 4], from: u16, to: u16) -> Vec<u8> {
        let mut r = from.to_be_bytes().to_vec();
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&to.to_be_bytes());
        let reference = isobmff_box(kind, &r, false);
        match meta_child(a, b"iref") {
            Some((meta, iref, end)) => avif_insert(a.to_vec(), end, &reference, &[meta, iref]),
            None => {
                let (meta, end, _) = child_boxes(a, 0, a.len())
                    .into_iter()
                    .find(|b| &b.2 == b"meta")
                    .unwrap();
                let iref = isobmff_box(b"iref", &[&[0, 0, 0, 0][..], &reference].concat(), false);
                avif_insert(a.to_vec(), end, &iref, &[meta])
            }
        }
    }

    /// `a` with item `id` given the same properties (`ipma` associations) as item `like`.
    fn avif_with_properties_of(a: &[u8], id: u16, like: u16) -> Vec<u8> {
        let (meta, iprp, _) = meta_child(a, b"iprp").unwrap();
        let ipma = child_boxes(a, iprp + 8, a.len())
            .into_iter()
            .find(|b| &b.2 == b"ipma")
            .unwrap();
        let body = ipma.0 + 8;
        assert_eq!(a[body], 0, "ipma version 0: 2-byte item IDs");
        let wide = a[body + 3] & 1 == 1;
        let count = u32::from_be_bytes(a[body + 4..body + 8].try_into().unwrap());
        let mut at = body + 8;
        let mut copied = None;
        for _ in 0..count {
            let item = u16::from_be_bytes([a[at], a[at + 1]]);
            let n = usize::from(a[at + 2]);
            let len = 3 + n * if wide { 2 } else { 1 };
            if item == like {
                let mut e = a[at..at + len].to_vec();
                e[..2].copy_from_slice(&id.to_be_bytes());
                copied = Some(e);
            }
            at += len;
        }
        let mut b = avif_insert(a.to_vec(), ipma.1, &copied.unwrap(), &[meta, iprp, ipma.0]);
        b[body + 4..body + 8].copy_from_slice(&(count + 1).to_be_bytes());
        b
    }

    /// `avif` with `n` more items, ids 2 up, each typed `av01` with no properties and one
    /// extent: `shared` appended once to `mdat` when given (every item pointing at it), else
    /// the primary's own bytes. The first `thumbs` are thumbnails of the primary. Every box
    /// is written whole, so making many items stays linear.
    fn avif_many_items(avif: &[u8], n: u32, shared: Option<&[u8]>, thumbs: u32) -> Vec<u8> {
        let top = child_boxes(avif, 0, avif.len());
        let (meta, meta_end, _) = *top.iter().find(|b| &b.2 == b"meta").unwrap();
        let (mdat, mdat_end, _) = *top.iter().find(|b| &b.2 == b"mdat").unwrap();
        assert_eq!(mdat_end, avif.len(), "mdat last");
        let children = child_boxes(avif, meta + 12, meta_end);
        let (iloc, iloc_end, _) = *children.iter().find(|b| &b.2 == b"iloc").unwrap();
        let (iinf, iinf_end, _) = *children.iter().find(|b| &b.2 == b"iinf").unwrap();
        let body = iloc + 8;
        assert_eq!((avif[body], avif[body + 4], avif[body + 5]), (0, 0x44, 0));
        assert_eq!(avif[iinf + 8], 0, "iinf version 0");
        let old_count = u16::from_be_bytes([avif[body + 6], avif[body + 7]]);
        let (_, primary_off, primary_len) = crate::fixtures::avif_extents(avif)[0];
        let infe = |id: u16| {
            let mut b = vec![2, 0, 0, 0];
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&[0, 0]);
            b.extend_from_slice(b"av01\0");
            isobmff_box(b"infe", &b, false)
        };
        let entry = |id: u16, off: u32, len: u32| {
            let mut e = id.to_be_bytes().to_vec();
            e.extend_from_slice(&[0, 0, 0, 1]);
            e.extend_from_slice(&off.to_be_bytes());
            e.extend_from_slice(&len.to_be_bytes());
            e
        };
        let ids = || (0..n).map(|i| u16::try_from(i + 2).unwrap());
        // Sizes first, so offsets can be known as the boxes are written.
        let iloc_len = (iloc_end - iloc) + 14 * n as usize;
        let iinf_len = (iinf_end - iinf) + infe(2).len() * n as usize;
        let mut refs = Vec::new();
        for id in ids().take(thumbs as usize) {
            refs.extend(isobmff_box(
                b"thmb",
                &[&id.to_be_bytes()[..], &[0, 1, 0, 1]].concat(),
                false,
            ));
        }
        let iref = isobmff_box(b"iref", &[&[0, 0, 0, 0][..], &refs].concat(), false);
        let grow = (iloc_len - (iloc_end - iloc)) + (iinf_len - (iinf_end - iinf)) + iref.len();
        let moved = |off: usize| (off + grow) as u32;
        let (off, len) = match shared {
            Some(d) => (moved(avif.len()), d.len() as u32),
            None => (moved(primary_off), primary_len as u32),
        };
        let mut iloc_body = avif[body..body + 6].to_vec();
        iloc_body.extend_from_slice(&(old_count as u32 + n).to_be_bytes()[2..]);
        let mut first = avif[body + 8..iloc_end].to_vec();
        // The primary's one extent offset, moved along with mdat.
        let at = first.len() - 8;
        first[at..at + 4].copy_from_slice(&moved(primary_off).to_be_bytes());
        iloc_body.extend(first);
        for id in ids() {
            iloc_body.extend(entry(id, off, len));
        }
        let mut iinf_body = avif[iinf + 8..iinf + 12].to_vec();
        let old_infes = u16::from_be_bytes([avif[iinf + 12], avif[iinf + 13]]);
        iinf_body.extend_from_slice(&(old_infes as u32 + n).to_be_bytes()[2..]);
        iinf_body.extend_from_slice(&avif[iinf + 14..iinf_end]);
        for id in ids() {
            iinf_body.extend(infe(id));
        }
        let mut meta_body = avif[meta + 8..meta + 12].to_vec();
        for &(start, end, kind) in &children {
            match &kind {
                b"iloc" => meta_body.extend(isobmff_box(b"iloc", &iloc_body, false)),
                b"iinf" => meta_body.extend(isobmff_box(b"iinf", &iinf_body, false)),
                _ => meta_body.extend_from_slice(&avif[start..end]),
            }
        }
        meta_body.extend(iref);
        let mut out = avif[..meta].to_vec();
        out.extend(isobmff_box(b"meta", &meta_body, false));
        out.extend_from_slice(&avif[meta_end..mdat]);
        let mut mdat_body = avif[mdat + 8..mdat_end].to_vec();
        if let Some(d) = shared {
            mdat_body.extend_from_slice(d);
        }
        out.extend(isobmff_box(b"mdat", &mdat_body, false));
        assert_eq!(out.len(), avif.len() + grow + shared.map_or(0, <[u8]>::len));
        out
    }

    /// An AVIF listing more items than any picture needs is refused before any of them is
    /// looked at; one with thousands, a hundred of them thumbnails sharing one large extent,
    /// is kept clean in well under a second.
    #[test]
    fn many_items_cost_little() {
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let want = pixels_of(&avif);
        let flood = avif_many_items(&avif, 60_000, None, 0);
        for (how, u) in settings() {
            let (res, took) = best_of_three(|| run(&flood, &u));
            assert!(matches!(res, Err(ImageError::Corrupt(_))), "{how}: {res:?}");
            assert!(took.as_millis() < 1000, "{how}: {took:?}");
        }
        let payload: Vec<u8> = TAIL.iter().copied().cycle().take(1_000_000).collect();
        let crowd = avif_many_items(&avif, 4_000, Some(&payload), 100);
        for (how, u) in settings() {
            let (out, took) = best_of_three(|| run(&crowd, &u));
            let out = out.unwrap_or_else(|e| panic!("{how}: {e}"));
            let kept = stored(&out);
            assert!(!has(&kept, b"<html>"), "{how}: the payload stayed");
            assert_eq!(pixels_of(&kept), want, "{how}");
            assert!(took.as_millis() < 1000, "{how}: {took:?}");
            println!("4,000 items, 100 thumbnails on 1 MB, {how}: {took:?}");
        }
    }

    /// What a hand-built AVIF takes from the 8×8 picture: its `ftyp`, `hdlr` and `pitm`
    /// boxes, its `ipco`, its `ipma` entries (version 0, narrow indices) and their count, how
    /// many associations they make, the index of its `pixi`, and its AV1 data.
    struct Picture {
        ftyp: Vec<u8>,
        hdlr: Vec<u8>,
        pitm: Vec<u8>,
        ipco: Vec<u8>,
        entries: Vec<u8>,
        count: u32,
        associations: usize,
        pixi: u8,
        av1: Vec<u8>,
        pixels: Vec<u8>,
    }

    fn picture() -> &'static Picture {
        static P: std::sync::OnceLock<Picture> = std::sync::OnceLock::new();
        P.get_or_init(|| {
            let a = encoded(&gradient(8, 8), Codec::Avif);
            let top = child_boxes(&a, 0, a.len());
            let whole = |list: &[(usize, usize, [u8; 4])], kind: &[u8; 4]| {
                let b = list.iter().find(|b| &b.2 == kind).unwrap();
                (b.0, b.1)
            };
            let (meta, meta_end) = whole(&top, b"meta");
            let inner = child_boxes(&a, meta + 12, meta_end);
            let (pitm, pitm_end) = whole(&inner, b"pitm");
            assert_eq!(
                &a[pitm + 8..pitm_end],
                &[0, 0, 0, 0, 0, 1],
                "pitm says item 1"
            );
            let (iprp, iprp_end) = whole(&inner, b"iprp");
            let props = child_boxes(&a, iprp + 8, iprp_end);
            let (ipco, ipco_end) = whole(&props, b"ipco");
            let (ipma, ipma_end) = whole(&props, b"ipma");
            assert_eq!(&a[ipma + 8..ipma + 12], &[0, 0, 0, 0], "ipma v0, narrow");
            let entries = a[ipma + 16..ipma_end].to_vec();
            let count = u32::from_be_bytes(a[ipma + 12..ipma + 16].try_into().unwrap());
            let mut at = 0;
            let mut associations = 0;
            for _ in 0..count {
                let n = usize::from(entries[at + 2]);
                associations += n;
                at += 3 + n;
            }
            let pixi = child_boxes(&a, ipco + 8, ipco_end)
                .iter()
                .position(|b| &b.2 == b"pixi")
                .unwrap();
            let (_, off, len) = crate::fixtures::avif_extents(&a)[0];
            let (ftyp, ftyp_end) = whole(&top, b"ftyp");
            let (hdlr, hdlr_end) = whole(&inner, b"hdlr");
            Picture {
                ftyp: a[ftyp..ftyp_end].to_vec(),
                hdlr: a[hdlr..hdlr_end].to_vec(),
                pitm: a[pitm..pitm_end].to_vec(),
                ipco: a[ipco..ipco_end].to_vec(),
                entries,
                count,
                associations,
                pixi: u8::try_from(pixi + 1).unwrap(),
                av1: a[off..off + len].to_vec(),
                pixels: pixels_of(&a),
            }
        })
    }

    /// An AVIF written whole around the 8×8 picture, as the reviewer's probes were: `ftyp`;
    /// a `meta` of the picture's `hdlr` and `pitm`, then `iloc`, `iinf`, `iref` and `iprp`
    /// from the fields below, then `meta_extra`; then `top`; then `mdat`, the picture's AV1
    /// data followed by `extra`. Extents are (offset into `mdat`'s body, length), item 1
    /// being the picture.
    struct Hand {
        iloc: Vec<(u16, Vec<(u32, u32)>)>,
        infe: Vec<(u16, [u8; 4])>,
        /// The count `iinf` declares, when it isn't the truth.
        declared: Option<u16>,
        /// `iref`'s children, written whole; `None` for no `iref`.
        iref: Option<Vec<u8>>,
        /// `ipma` entries after the picture's: (item, property indices).
        ipma: Vec<(u16, Vec<u8>)>,
        /// Property boxes after the picture's, in its `ipco`.
        ipco_extra: Vec<u8>,
        /// Boxes inside `iprp`, after its `ipco` and `ipma`.
        iprp_extra: Vec<u8>,
        meta_extra: Vec<u8>,
        top: Vec<u8>,
        extra: Vec<u8>,
    }

    impl Hand {
        fn new() -> Self {
            Hand {
                iloc: vec![(1, vec![(0, picture().av1.len() as u32)])],
                infe: vec![(1, *b"av01")],
                declared: None,
                iref: None,
                ipma: Vec::new(),
                ipco_extra: Vec::new(),
                iprp_extra: Vec::new(),
                meta_extra: Vec::new(),
                top: Vec::new(),
                extra: Vec::new(),
            }
        }

        fn meta(&self, base: u32) -> Vec<u8> {
            let p = picture();
            let mut iloc = vec![0, 0, 0, 0, 0x44, 0];
            iloc.extend((self.iloc.len() as u16).to_be_bytes());
            for (id, extents) in &self.iloc {
                iloc.extend(id.to_be_bytes());
                iloc.extend([0, 0]);
                iloc.extend((extents.len() as u16).to_be_bytes());
                for &(off, len) in extents {
                    iloc.extend((base + off).to_be_bytes());
                    iloc.extend(len.to_be_bytes());
                }
            }
            let mut iinf = vec![0, 0, 0, 0];
            iinf.extend(
                self.declared
                    .unwrap_or(self.infe.len() as u16)
                    .to_be_bytes(),
            );
            for (id, kind) in &self.infe {
                let body = [&[2, 0, 0, 0][..], &id.to_be_bytes(), &[0, 0], kind, &[0]].concat();
                iinf.extend(isobmff_box(b"infe", &body, false));
            }
            let mut ipma = vec![0, 0, 0, 0];
            ipma.extend((p.count + self.ipma.len() as u32).to_be_bytes());
            ipma.extend(&p.entries);
            for (id, indices) in &self.ipma {
                ipma.extend(id.to_be_bytes());
                ipma.push(u8::try_from(indices.len()).unwrap());
                ipma.extend(indices);
            }
            let ipco = [&p.ipco[8..], &self.ipco_extra[..]].concat();
            let iprp = [
                &isobmff_box(b"ipco", &ipco, false)[..],
                &isobmff_box(b"ipma", &ipma, false),
                &self.iprp_extra,
            ]
            .concat();
            let mut body = vec![0, 0, 0, 0];
            body.extend(&p.hdlr);
            body.extend(&p.pitm);
            body.extend(isobmff_box(b"iloc", &iloc, false));
            body.extend(isobmff_box(b"iinf", &iinf, false));
            if let Some(refs) = &self.iref {
                body.extend(isobmff_box(
                    b"iref",
                    &[&[0, 0, 0, 0][..], refs].concat(),
                    false,
                ));
            }
            body.extend(isobmff_box(b"iprp", &iprp, false));
            body.extend(&self.meta_extra);
            isobmff_box(b"meta", &body, false)
        }

        fn build(&self) -> Vec<u8> {
            let p = picture();
            let base = p.ftyp.len() + self.meta(0).len() + self.top.len() + 8;
            let mut out = p.ftyp.clone();
            out.extend(self.meta(u32::try_from(base).unwrap()));
            out.extend(&self.top);
            out.extend(isobmff_box(
                b"mdat",
                &[&p.av1[..], &self.extra].concat(),
                false,
            ));
            out
        }
    }

    /// An `iref` child of `kind` from item `from` to each of `to`.
    fn reference(kind: &[u8; 4], from: u16, to: &[u16], large: bool) -> Vec<u8> {
        let mut body = from.to_be_bytes().to_vec();
        body.extend((to.len() as u16).to_be_bytes());
        for t in to {
            body.extend(t.to_be_bytes());
        }
        isobmff_box(kind, &body, large)
    }

    /// `bytes` refused as corrupt in every setting, each in under a second.
    fn refused_fast(bytes: &[u8], case: &str) {
        for (how, u) in settings() {
            let (res, took) = best_of_three(|| run(bytes, &u));
            assert!(
                matches!(res, Err(ImageError::Corrupt(_))),
                "{case}, {how}: {res:?}"
            );
            assert!(took.as_millis() < 1000, "{case}, {how}: {took:?}");
        }
    }

    /// What `f` gives and how long it took, the best of up to three runs, stopping at the
    /// first under a second: a busy machine can slow one run of cheap work, but work that
    /// grows with its input is slow every time.
    fn best_of_three<T>(mut f: impl FnMut() -> T) -> (T, std::time::Duration) {
        let mut best: Option<(T, std::time::Duration)> = None;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let out = f();
            let took = started.elapsed();
            if best.as_ref().is_none_or(|(_, b)| took < *b) {
                best = Some((out, took));
            }
            if took.as_millis() < 1000 {
                break;
            }
        }
        best.expect("one run at least")
    }

    /// `bytes` stored in every setting with the picture's own pixels and none of `TAIL`,
    /// each in under a second.
    fn kept_fast(bytes: &[u8], case: &str) {
        kept_within(bytes, case, Some(1000));
    }

    /// `bytes` stored as `kept_fast` asks, each upload in under `millis` when given.
    fn kept_within(bytes: &[u8], case: &str, millis: Option<u128>) {
        for (how, u) in settings() {
            let (out, took) = best_of_three(|| run(bytes, &u));
            let out = out.unwrap_or_else(|e| panic!("{case}, {how}: {e}"));
            let kept = stored(&out);
            assert!(!has(&kept, b"<html>"), "{case}, {how}: the payload stayed");
            assert_eq!(pixels_of(&kept), picture().pixels, "{case}, {how}");
            if let Some(millis) = millis {
                assert!(took.as_millis() < millis, "{case}, {how}: {took:?}");
            }
        }
    }

    /// `ipma` entries, ids from 2, making `n` associations to `pixi` in all.
    fn associations(n: usize) -> Vec<(u16, Vec<u8>)> {
        let pixi = picture().pixi;
        (0..n)
            .step_by(255)
            .enumerate()
            .map(|(i, at)| (2 + i as u16, vec![pixi; (n - at).min(255)]))
            .collect()
    }

    #[test]
    fn a_hand_built_avif_is_kept() {
        kept_fast(&Hand::new().build(), "baseline");
    }

    /// avif-parse looks through every association for each `auxl` reference to the picture,
    /// so both are bounded: 16 references and 16,384 associations at most.
    #[test]
    fn an_alpha_search_stays_small() {
        let p = picture();
        // The reviewer's shape: 1,000 references, 1,000 entries of 255 associations.
        let mut h = Hand::new();
        h.iref = Some(reference(b"auxl", 2, &[1; 1000], false));
        h.ipma = (2..1002).map(|id| (id, vec![p.pixi; 255])).collect();
        refused_fast(&h.build(), "1,000 auxl × 1,000×255");
        let at_caps = |refs: usize, total: usize| {
            let mut h = Hand::new();
            h.iref = Some(reference(b"auxl", 2, &vec![1; refs], false));
            h.ipma = associations(total - p.associations);
            h.build()
        };
        kept_fast(&at_caps(16, 16_384), "at both caps");
        refused_fast(&at_caps(17, 16_384), "17 auxl");
        refused_fast(&at_caps(16, 16_385), "16,385 associations");
    }

    /// Each list avif-parse walks is counted as it's walked: every `infe` there, whatever
    /// `iinf` declares; one `iinf`, `iloc`, `iref`, `iprp`, `ipma` and `ipco` only; `iref`'s
    /// children followed through 64-bit sizes.
    #[test]
    fn no_list_goes_uncounted() {
        let ids = || 2..60_002u16;
        let mut lying = Hand::new();
        lying.infe.extend(ids().map(|id| (id, *b"av01")));
        lying.declared = Some(2);
        refused_fast(&lying.build(), "iinf says 2, holds 60,001");
        let thumbs: Vec<u8> = ids()
            .flat_map(|id| reference(b"thmb", id, &[1], false))
            .collect();
        let mut second = Hand::new();
        second.iref = Some(Vec::new());
        second.meta_extra = isobmff_box(b"iref", &[&[0, 0, 0, 0][..], &thumbs].concat(), false);
        refused_fast(&second.build(), "a second iref of 60,000");
        let mut wide = Hand::new();
        wide.iref = Some([reference(b"thmb", 2, &[1], true), thumbs].concat());
        refused_fast(&wide.build(), "a 64-bit iref child, then 60,000");
        let mut entries = vec![0, 0, 0, 0];
        entries.extend(60_000u32.to_be_bytes());
        for id in ids().take(60_000) {
            entries.extend(id.to_be_bytes());
            entries.extend([1, picture().pixi]);
        }
        let ipma = isobmff_box(b"ipma", &entries, false);
        let mut iprp = Hand::new();
        iprp.meta_extra = isobmff_box(b"iprp", &ipma, false);
        refused_fast(&iprp.build(), "a second iprp of 60,000");
        let mut inside = Hand::new();
        inside.iprp_extra = ipma;
        refused_fast(&inside.build(), "a second ipma of 60,000");
        // One of each, however small the second.
        let meta_child = |kind: &[u8; 4]| {
            let first = Hand::new().meta(0);
            let inner = child_boxes(&first, 12, first.len());
            let b = inner.iter().find(|b| &b.2 == kind).unwrap();
            first[b.0..b.1].to_vec()
        };
        for kind in [b"iinf", b"iloc", b"iprp"] {
            let mut h = Hand::new();
            h.meta_extra = meta_child(kind);
            refused_fast(&h.build(), &format!("a second {}", kind.escape_ascii()));
        }
        let mut h = Hand::new();
        h.iref = Some(Vec::new());
        h.meta_extra = isobmff_box(b"iref", &[0, 0, 0, 0], false);
        refused_fast(&h.build(), "a second empty iref");
        let mut h = Hand::new();
        h.iprp_extra = picture().ipco.clone();
        refused_fast(&h.build(), "a second ipco");
    }

    /// `keep` on `bytes`, in each of the four ways it's called, each in under a second.
    fn kept_by_keep(bytes: &[u8], case: &str) -> Vec<Result<Option<Vec<u8>>, ImageError>> {
        [
            (false, 0),
            (true, 0),
            (false, 50_000_000),
            (true, 50_000_000),
        ]
        .into_iter()
        .map(|(keep_metadata, budget)| {
            let (kept, took) = best_of_three(|| avif::keep(bytes, keep_metadata, budget));
            assert!(
                took.as_millis() < 1000,
                "{case}, {keep_metadata}/{budget}: {took:?}"
            );
            kept
        })
        .collect()
    }

    /// An AVIF has 64 top-level boxes at most, 8 of them `mdat`: avif-parse looks through
    /// every `mdat` for each of the picture's extents, and `keep` finds the box each
    /// zero-length extent ends in. Under the cap, 65,472 zero-length extents cost `keep`
    /// what a few do; over it, the file is refused before any is read.
    #[test]
    fn top_level_boxes_are_few() {
        let free = |n: usize| isobmff_box(b"free", &[], false).repeat(n);
        let zero_length = |top: Vec<u8>| {
            let mut h = Hand::new();
            h.top = top;
            h.extra = vec![0; 16];
            let last = (picture().av1.len() + 15) as u32;
            h.iloc
                .extend((2..66u16).map(|id| (id, vec![(last, 0); 1023])));
            h.build()
        };
        // ftyp, meta and mdat, and 61 more: 64.
        let at_cap = zero_length(free(61));
        for kept in kept_by_keep(&at_cap, "64 boxes, 65,472 zero-length extents") {
            assert!(matches!(kept, Ok(Some(_))), "{kept:?}");
        }
        // Timed above as `keep` alone; the whole upload is linear work a loaded runner may
        // stretch, so it's checked for what it keeps, not how long.
        kept_within(&at_cap, "64 boxes, 65,472 zero-length extents", None);
        for (case, file) in [
            ("65 boxes", zero_length(free(62))),
            ("100,000 free boxes", zero_length(free(100_000))),
        ] {
            for kept in kept_by_keep(&file, case) {
                assert!(
                    matches!(kept, Err(ImageError::Corrupt(_))),
                    "{case}: {kept:?}"
                );
            }
            refused_fast(&file, case);
        }
        // The reviewer's: 100,000 one-byte mdats, the picture in 1,024 extents in the last.
        let mdats = |n: usize| {
            let mut h = Hand::new();
            h.top = isobmff_box(b"mdat", &[0], false).repeat(n);
            h.extra = vec![0; 1024];
            let av1 = picture().av1.len() as u32;
            h.iloc[0].1.extend((0..1023).map(|i| (av1 + i, 1)));
            h.build()
        };
        refused_fast(&mdats(100_000), "100,000 mdats");
        refused_fast(&mdats(8), "9 mdats");
        let mut eight = Hand::new();
        eight.top = isobmff_box(b"mdat", &[0], false).repeat(7);
        kept_fast(&eight.build(), "8 mdats");
    }

    /// What's kept is copied once a byte, whatever lists it: one item named by 2,047 `cdsc`
    /// references, or 2,047 items over the same 24 MB.
    #[test]
    fn kept_bytes_are_copied_once() {
        let body = (picture().av1.len() + 24_000_000) as u32;
        let mut one = Hand::new();
        one.extra = vec![0; 24_000_000];
        one.iloc.push((2, vec![(0, body)]));
        one.iref = Some(
            (0..2047)
                .flat_map(|_| reference(b"cdsc", 2, &[1], false))
                .collect(),
        );
        kept_fast(&one.build(), "one item, 2,047 cdsc");
        let mut many = Hand::new();
        many.extra = vec![0; 24_000_000];
        many.iloc.extend((2..2049).map(|id| (id, vec![(0, body)])));
        many.iref = Some(
            (2..2049)
                .flat_map(|id| reference(b"cdsc", id, &[1], false))
                .collect(),
        );
        kept_fast(&many.build(), "2,047 items over 24 MB");
    }

    /// What the loop audit found: an item `iloc` lists twice (avif-parse joins every entry
    /// an id has), and a large `auxC` associated many times (avif-parse copies it for each
    /// association, then reads every copy for each `auxl` reference).
    #[test]
    fn audited_loops_are_bounded() {
        let av1 = picture().av1.len() as u32;
        let mut twice = Hand::new();
        twice.extra = vec![0; 1_000_000];
        twice
            .iloc
            .extend((0..999).map(|_| (1, vec![(av1, 1_000_000)])));
        refused_fast(&twice.build(), "the picture located 1,000 times");
        let mut again = Hand::new();
        again.iloc.push((1, vec![(0, av1)]));
        refused_fast(&again.build(), "the picture located twice");
        let index = child_boxes(&picture().ipco, 8, picture().ipco.len()).len() as u8 + 1;
        let aux = |len: usize, n: usize| {
            let mut h = Hand::new();
            let body = [&[0, 0, 0, 0][..], &vec![b'u'; len]].concat();
            h.ipco_extra = isobmff_box(b"auxC", &body, false);
            h.ipma = (0..n)
                .step_by(255)
                .map(|at| (2, vec![index; (n - at).min(255)]))
                .collect();
            h.iref = Some(reference(b"auxl", 2, &[1; 16], false));
            h.build()
        };
        refused_fast(&aux(100_000, 2_040), "100 KB auxC × 2,040");
        refused_fast(&aux(65_536, 1), "one auxC over 64 KB");
        kept_fast(&aux(100, 255), "100-byte auxC × 255");
    }

    /// An item whose extents add up to more than the file is refused before any is copied:
    /// overlapping extents can't make a file cost its size a thousand times over.
    #[test]
    fn overlapping_extents_cant_multiply_a_file() {
        let p = picture();
        let payload: Vec<u8> = TAIL.iter().copied().cycle().take(1_000_000).collect();
        let body = (p.av1.len() + payload.len()) as u32;
        let mut h = Hand::new();
        h.extra = payload;
        h.iloc[0].1.extend(vec![(0, body); 1023]);
        refused_fast(&h.build(), "1,024 extents over 1 MB");
        h.iloc[0].1.truncate(3);
        refused_fast(&h.build(), "the picture, then mdat twice");
        // A thumbnail's too, though it's never looked at past its first extent.
        let mut h = Hand::new();
        h.extra = vec![0; 4096];
        let body = (p.av1.len() + h.extra.len()) as u32;
        h.iloc.push((2, vec![(0, body); 2]));
        h.iref = Some(reference(b"thmb", 2, &[1], false));
        refused_fast(&h.build(), "a thumbnail over mdat twice");
    }

    /// `meta` holds 64 boxes at most and `ipco` 4,096: every pass over them is linear,
    /// but empty boxes fit about 3M to 25 MB, and several passes run per upload.
    #[test]
    fn meta_and_ipco_hold_few_boxes() {
        let free = |n: usize| isobmff_box(b"free", &[], false).repeat(n);
        let mut h = Hand::new();
        h.meta_extra = free(2_900_000);
        refused_fast(&h.build(), "2.9M boxes in meta");
        let mut h = Hand::new();
        h.ipco_extra = free(2_900_000);
        refused_fast(&h.build(), "2.9M boxes in ipco");
        // hdlr, pitm, iloc, iinf and iprp, and 59 more: 64.
        let mut h = Hand::new();
        h.meta_extra = free(59);
        kept_fast(&h.build(), "64 boxes in meta");
        h.meta_extra = free(60);
        refused_fast(&h.build(), "65 boxes in meta");
        let have = child_boxes(&picture().ipco, 8, picture().ipco.len()).len();
        let mut h = Hand::new();
        h.ipco_extra = free(4_096 - have);
        kept_fast(&h.build(), "4,096 boxes in ipco");
        h.ipco_extra = free(4_097 - have);
        refused_fast(&h.build(), "4,097 boxes in ipco");
    }

    /// Hand-built AVIFs chosen by a fixed seed. Each case makes one dimension large, up to
    /// past its cap, and keeps the others to a handful, so no cap hides another: payload
    /// size, the zeros after it, items, extents per item, `infe` boxes, references and their
    /// targets, `cdsc` references to one item in many extents, `ipma` entries and their
    /// associations, the picture in many one-byte extents behind many `mdat`s, the picture
    /// located many times, top-level boxes, a large `auxC`, and boxes in `meta` and in
    /// `ipco`. Lying counts, second boxes
    /// and 64-bit sizes come and go at random. Each file is kept (or refused) and parsed, as
    /// the probe does, in under a second, and nothing kept holds the payload.
    #[test]
    fn hand_built_avifs_stay_cheap() {
        use super::walker_fuzz::Rng;
        const CASES: usize = 300;
        let p = picture();
        let mut rng = Rng(0x5eed_a11f_0b0e_7a11);
        // Up to about `max`, as often small as large.
        let some = |rng: &mut Rng, max: usize| {
            let bits = rng.n(usize::BITS as usize - max.leading_zeros() as usize + 1);
            rng.n(1 << bits).min(max)
        };
        let av1 = p.av1.len() as u32;
        let ipco = child_boxes(&p.ipco, 8, p.ipco.len()).len() as u8;
        let started = std::time::Instant::now();
        for case in 0..CASES {
            let big = case % 15;
            let max = |d: usize, large: usize| if d == big { large } else { 8 };
            let mut h = Hand::new();
            // Payload, for items that must never be kept; zeros after it, for those that may.
            let payload = some(&mut rng, max(0, 1_000_000)).max(64);
            h.extra = TAIL.iter().copied().cycle().take(payload).collect();
            let zeros = match big {
                1 => some(&mut rng, 24_000_000).max(64),
                // Large enough to cost when copied a thousand times.
                7 => 12_000_000 + rng.n(12_000_000),
                9 => 4_096,
                10 => 1_000_000 + rng.n(3_000_000),
                _ => some(&mut rng, 64).max(64),
            };
            h.extra.extend(vec![0; zeros]);
            let body = av1 + payload as u32;
            let extent = |rng: &mut Rng| {
                let off = av1 + rng.n(payload) as u32;
                let len = [0, 1, rng.n((body - off) as usize + 1) as u32, body - off][rng.n(4)];
                (off, len)
            };
            let items = some(&mut rng, max(2, 5_000));
            for id in 0..items {
                let n = some(&mut rng, max(3, 1_024)).max(1);
                let extents = (0..n).map(|_| extent(&mut rng)).collect();
                h.iloc.push((2 + id as u16, extents));
            }
            let kinds = [*b"av01", *b"hvc1", *b"jpeg", *b"grid"];
            let named = some(&mut rng, max(4, 5_000));
            h.infe
                .extend((0..named).map(|i| (2 + i as u16, kinds[rng.n(kinds.len())])));
            if rng.n(4) == 0 {
                h.declared = Some(rng.n(8) as u16);
            }
            let ref_kinds = [b"thmb", b"auxl", b"dimg", b"prem"];
            let mut refs = Vec::new();
            for _ in 0..some(&mut rng, max(5, 5_000)) {
                let kind = ref_kinds[rng.n(ref_kinds.len())];
                let from = 2 + rng.n(items.max(1)) as u16;
                let to = vec![1; some(&mut rng, max(6, 4_000)).max(1)];
                refs.extend(reference(kind, from, &to, rng.n(16) == 0));
            }
            // Metadata named by many `cdsc` references, in many extents over the zeros.
            if big == 7 || rng.n(4) == 0 {
                let n = some(&mut rng, max(3, 1_024)).max(1);
                let over = (0..n)
                    .map(|_| {
                        let off = rng.n(zeros);
                        let len = [0, rng.n(zeros - off), zeros - off][rng.n(3)];
                        (body + off as u32, len as u32)
                    })
                    .collect();
                h.iloc.push((60_000, over));
                let named = if big == 7 {
                    1_000 + rng.n(1_047)
                } else {
                    some(&mut rng, 8)
                };
                for _ in 0..named {
                    refs.extend(reference(b"cdsc", 60_000, &[1], false));
                }
            }
            h.iref = (big == 7 || rng.n(4) != 0).then_some(refs);
            let entries = some(&mut rng, max(8, 4_000));
            h.ipma = (0..entries)
                .map(|i| {
                    let n = some(&mut rng, max(8, 255));
                    let indices = (0..n).map(|_| rng.n(ipco as usize + 2) as u8).collect();
                    (2 + i as u16, indices)
                })
                .collect();
            h.top = match rng.n(3) {
                0 => isobmff_box(b"mdat", &[0], false).repeat(some(&mut rng, max(11, 100_000))),
                1 => isobmff_box(b"free", &[], false).repeat(some(&mut rng, max(11, 100_000))),
                _ => isobmff_box(b"free", &[], false).repeat(some(&mut rng, 64)),
            };
            match big {
                // The picture in many one-byte extents, behind many `mdat`s.
                9 => {
                    let n = 512 + rng.n(511) as u32;
                    h.iloc[0]
                        .1
                        .extend((0..n).map(|i| (body + i % zeros as u32, 1)));
                    let mdats = 20_000 + rng.n(80_000);
                    h.top = isobmff_box(b"mdat", &[0], false).repeat(mdats);
                }
                // The picture, or another item, located many times over the zeros.
                10 => {
                    let id = [1, 1, 2][rng.n(3)];
                    let whole = vec![(body, zeros as u32)];
                    h.iloc.extend(vec![(id, whole); 500 + rng.n(500)]);
                }
                // A large `auxC`, associated many times, and alpha references to search it.
                12 => {
                    let len = 50_000 + rng.n(150_000);
                    let aux = [&[0, 0, 0, 0][..], &vec![b'u'; len]].concat();
                    h.ipco_extra = isobmff_box(b"auxC", &aux, false);
                    let n = 1_000 + rng.n(3_000);
                    h.ipma = (0..n)
                        .step_by(255)
                        .map(|at| (2, vec![ipco + 1; (n - at).min(255)]))
                        .collect();
                    let alpha = vec![1; some(&mut rng, 32).max(1)];
                    h.iref = Some(reference(b"auxl", 2, &alpha, false));
                }
                // Many boxes in `meta`, or in `ipco`.
                13 => h.meta_extra = isobmff_box(b"free", &[], false).repeat(1 + rng.n(2_900_000)),
                14 => h.ipco_extra = isobmff_box(b"free", &[], false).repeat(1 + rng.n(2_900_000)),
                _ => {
                    let again = some(&mut rng, 8);
                    h.iloc[0].1.extend(vec![(0, av1); again]);
                }
            }
            match rng.n(6) {
                _ if big == 13 => {}
                0 => h.meta_extra = isobmff_box(b"iref", &[0, 0, 0, 0], false),
                1 => h.meta_extra = isobmff_box(b"free", &[0; 16], false),
                2 => h.iprp_extra = isobmff_box(b"ipma", &[0; 8], false),
                _ => {}
            }
            let file = h.build();
            for (keep_metadata, budget) in [
                (false, 0),
                (true, 0),
                (false, 50_000_000),
                (true, 50_000_000),
            ] {
                let ((), took) = best_of_three(|| {
                    let kept = avif::keep(&file, keep_metadata, budget);
                    if let Ok(Some(k)) = &kept {
                        assert!(!has(k, b"<html>"), "case {case}: the payload stayed");
                    }
                    if let Ok(k) = &kept
                        && let Ok(parsed) = avif::parse(k.as_deref().unwrap_or(&file))
                    {
                        let _ = avif::decode_bound(&parsed.primary_item);
                    }
                });
                assert!(
                    took.as_millis() < 1000,
                    "case {case} (large: {big}, {} bytes): {took:?}",
                    file.len()
                );
            }
        }
        println!("{CASES} hand-built AVIFs × 4: {:?}", started.elapsed());
    }

    /// Thumbnails share one budget, what the cap leaves after the picture: at most one is
    /// decoded, the first that's an AV1 picture no larger than the primary and fitting what's
    /// left; every other is zeroed undecoded.
    #[test]
    fn thumbnails_share_one_budget() {
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let want = pixels_of(&avif);
        let (_, off, len) = crate::fixtures::avif_extents(&avif)[0];
        let primary_bytes = avif[off..off + len].to_vec();
        let thumbnail = |a: &[u8], id: u16| {
            avif_with_ref(
                &avif_with_properties_of(
                    &avif_with_item(a, id, b"av01", None, &primary_bytes, false),
                    id,
                    1,
                ),
                b"thmb",
                id,
                1,
            )
        };
        // Twenty, every other one pointing at the bytes of the one before it.
        let mut many = avif.clone();
        for id in 80..100u16 {
            many = thumbnail(&many, id);
            if id % 2 == 1 {
                let extents = crate::fixtures::avif_extents(&many);
                let (_, before, _) = *extents.iter().find(|e| e.0 == u32::from(id - 1)).unwrap();
                let index = extents.iter().position(|e| e.0 == u32::from(id)).unwrap();
                many = crate::fixtures::avif_set_extent(many, index, before as u32, len as u32);
            }
        }
        let copies = |k: &[u8]| k.windows(len).filter(|w| *w == &primary_bytes[..]).count();
        let limited = |pixels: u64| {
            upload(serde_json::json!({
                "constructorOptions": {"limitInputPixels": pixels}
            }))
        };
        // 200 leaves 136 after the 64-pixel picture: room for one 8×8 thumbnail.
        let started = std::time::Instant::now();
        let kept = stored(&run(&many, &limited(200)).unwrap());
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
        assert_eq!(copies(&kept), 2, "the picture and one thumbnail");
        assert_eq!(pixels_of(&kept), want);
        // 100 leaves 36: no thumbnail fits, so it's zeroed, and the file is still stored.
        let one = thumbnail(&avif, 98);
        let kept = stored(&run(&one, &limited(100)).unwrap());
        assert_eq!(copies(&kept), 1, "the picture alone");
        assert_eq!(pixels_of(&kept), want);
        assert_eq!(copies(&stored(&run(&one, &limited(200)).unwrap())), 2);
        // Only the first four thumbnails are looked at: four that aren't pictures, then a
        // real one, and the real one is zeroed unexamined.
        let mut fifth = avif.clone();
        for id in 90..94u16 {
            fifth = avif_with_ref(
                &avif_with_item(&fifth, id, b"mime", Some("text/html"), TAIL, false),
                b"thmb",
                id,
                1,
            );
        }
        let fifth = thumbnail(&fifth, 98);
        let kept = stored(&run(&fifth, &upload(serde_json::json!({}))).unwrap());
        assert_eq!(copies(&kept), 1, "the fifth thumbnail was looked at");
        assert!(!has(&kept, b"<html>"));
        // A "thumbnail" larger than the picture isn't one, whatever the budget.
        let big = encoded(&gradient(16, 16), Codec::Avif);
        let (_, boff, blen) = crate::fixtures::avif_extents(&big)[0];
        let big_bytes = &big[boff..boff + blen];
        let larger = avif_with_ref(
            &avif_with_properties_of(
                &avif_with_item(&avif, 98, b"av01", None, big_bytes, false),
                98,
                1,
            ),
            b"thmb",
            98,
            1,
        );
        let kept = stored(&run(&larger, &upload(serde_json::json!({}))).unwrap());
        assert!(!has(&kept, big_bytes), "the larger one stayed");
        assert_eq!(pixels_of(&kept), want);
    }

    /// Items an AVIF holds beside its picture: an HTML page in `idat` goes, and so does
    /// anything merely called a picture; its alpha, a real thumbnail of it, and (with
    /// `withMetadata`) any metadata describing it stay.
    #[test]
    fn an_avif_keeps_only_what_belongs_to_its_picture() {
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let want = pixels_of(&avif);
        let in_idat = avif_with_item(&avif, 99, b"mime", Some("text/html"), TAIL, true);
        let rgba = DynamicImage::ImageRgba8(::image::RgbaImage::from_fn(8, 8, |x, y| {
            ::image::Rgba([x as u8 * 30, y as u8 * 30, 90, (x * y * 4) as u8])
        }));
        let transparent = encoded(&rgba, Codec::Avif);
        // A real thumbnail: the picture's own AV1 bytes as item 98, with its properties.
        let (_, off, len) = crate::fixtures::avif_extents(&avif)[0];
        let primary_bytes = avif[off..off + len].to_vec();
        let thumbed = avif_with_ref(
            &avif_with_properties_of(
                &avif_with_item(&avif, 98, b"av01", None, &primary_bytes, false),
                98,
                1,
            ),
            b"thmb",
            98,
            1,
        );
        let manifest = b"C2PA-MANIFEST-BYTES";
        let described = avif_with_ref(
            &avif_with_item(
                &avif,
                97,
                b"mime",
                Some("application/c2pa"),
                manifest,
                false,
            ),
            b"cdsc",
            97,
            1,
        );
        for (how, u) in settings() {
            let at = |name: &str| format!("{name}, {how}");
            let kept = |bytes: &[u8]| {
                let out = run(bytes, &u).unwrap_or_else(|e| panic!("{how}: {e}"));
                stored(&out)
            };
            let k = kept(&in_idat);
            assert!(!has(&k, b"<html>"), "{}: the page stayed", at("idat item"));
            assert_eq!(pixels_of(&k), want, "{}", at("idat item"));
            let k = kept(&transparent);
            assert_eq!(
                pixels_of(&k),
                pixels_of(&transparent),
                "{}: alpha",
                at("rgba")
            );
            let k = kept(&thumbed);
            let copies = k.windows(len).filter(|w| *w == &primary_bytes[..]).count();
            assert_eq!(copies, 2, "{}: the thumbnail went", at("thmb"));
            assert_eq!(pixels_of(&k), want, "{}", at("thmb"));
            let k = kept(&described);
            assert_eq!(has(&k, manifest), u.with_metadata, "{}", at("cdsc"));
            assert_eq!(pixels_of(&k), want, "{}", at("cdsc"));
        }
        // Nothing is a picture for being called one: an HTML page declared a thumbnail, an
        // alpha (without the alpha `auxC`) or a tile of an AV1 primary, an alpha hung off a
        // real thumbnail, and an `av01` thumbnail holding no AV1.
        let page = |id| avif_with_item(&avif, id, b"mime", Some("text/html"), TAIL, true);
        let off_thumbnail = avif_with_ref(
            &avif_with_item(&thumbed, 99, b"mime", Some("text/html"), TAIL, true),
            b"auxl",
            99,
            98,
        );
        let fake = avif_with_ref(
            &avif_with_properties_of(
                &avif_with_item(&avif, 96, b"av01", None, TAIL, false),
                96,
                1,
            ),
            b"thmb",
            96,
            1,
        );
        let posing: [(&str, Vec<u8>); 5] = [
            (
                "page as a thumbnail",
                avif_with_ref(&page(99), b"thmb", 99, 1),
            ),
            ("page as an alpha", avif_with_ref(&page(99), b"auxl", 99, 1)),
            ("page as a tile", avif_with_ref(&page(99), b"dimg", 1, 99)),
            ("page as a thumbnail's alpha", off_thumbnail),
            ("av01 thumbnail of no AV1", fake),
        ];
        for (name, bytes) in posing {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let k = stored(&run(&bytes, &u).unwrap_or_else(|e| panic!("{at}: {e}")));
                assert!(!has(&k, b"<html>"), "{at}: the page stayed");
                assert_eq!(pixels_of(&k), want, "{at}");
                if name.ends_with("thumbnail's alpha") {
                    let copies = k.windows(len).filter(|w| *w == &primary_bytes[..]).count();
                    assert_eq!(copies, 2, "{at}: the real thumbnail went");
                }
            }
        }

        // The alpha is a picture of its own: zeroed, the pixels would differ.
        assert_ne!(
            pixels_of(&transparent),
            pixels_of(&encoded(
                &DynamicImage::ImageRgb8(rgba.to_rgb8()),
                Codec::Avif
            ))
        );
    }

    /// What no reader of the picture takes from `meta` keeps only its header: `hdlr`'s name,
    /// `dinf`, `grpl`, and every property but those avif-parse reads.
    #[test]
    fn avif_structure_carries_no_payload() {
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let want = pixels_of(&avif);
        let (meta, meta_end, _) = child_boxes(&avif, 0, avif.len())
            .into_iter()
            .find(|b| &b.2 == b"meta")
            .unwrap();
        let hdlr = child_boxes(&avif, meta + 12, meta_end)
            .into_iter()
            .find(|b| &b.2 == b"hdlr")
            .unwrap();
        let at_end = |kind: &[u8; 4]| {
            avif_insert(
                avif.clone(),
                meta_end,
                &isobmff_box(kind, TAIL, false),
                &[meta],
            )
        };
        let property = |kind: &[u8; 4]| {
            crate::fixtures::avif_with_property(8, 8, &isobmff_box(kind, TAIL, false))
        };
        for (name, bytes) in [
            (
                "hdlr name",
                avif_insert(avif.clone(), hdlr.0 + 32, TAIL, &[meta, hdlr.0]),
            ),
            ("dinf", at_end(b"dinf")),
            ("grpl", at_end(b"grpl")),
            ("unknown property", property(b"zzzz")),
            ("free property", property(b"free")),
            ("uuid property", property(b"uuid")),
        ] {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let out = run(&bytes, &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                let kept = stored(&out);
                assert_eq!(out.mime, "image/avif", "{at}");
                assert!(!has(&kept, b"<html>"), "{at}: the payload stayed");
                assert_eq!(pixels_of(&kept), want, "{at}: the picture changed");
            }
        }
        // What the picture is read from stays: a rotation still turns it.
        let turned = crate::fixtures::avif_rotated(8, 4, 1);
        for (how, u) in settings() {
            let out = run(&turned, &u).unwrap();
            assert_eq!((out.width, out.height), (4, 8), "{how}");
        }
    }

    /// What's inside an AVIF's `meta` and among its items is kept only where it's the
    /// picture's; a TIFF's gaps go; an extent of length 0 ends with its box.
    #[test]
    fn what_no_part_of_the_picture_reaches_goes() {
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let want = pixels_of(&avif);
        let extents = crate::fixtures::avif_extents(&avif);
        assert_eq!(extents.len(), 1, "one item, one extent");
        let to_box_end = tailed(crate::fixtures::avif_set_extent(
            avif.clone(),
            0,
            extents[0].1 as u32,
            0,
        ));
        for (name, bytes) in [
            ("avif free box inside meta", avif_free_in_meta(&avif)),
            ("avif extra item", avif_extra_item(&avif)),
            ("avif extent of length 0", to_box_end),
        ] {
            assert!(
                probe(&file(&dir(), &bytes), &bytes[..8192.min(bytes.len())]).is_ok(),
                "{name}"
            );
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let out = run(&bytes, &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                let kept = stored(&out);
                assert!(!has(&kept, b"<html>"), "{at}: the payload stayed");
                assert_eq!(pixels_of(&kept), want, "{at}: the picture changed");
            }
        }

        // A payload in a gap a moved IFD0 leaves, and the same TIFF big-endian.
        let tiff = tiff_with(true, 1);
        let moved = tiff_moved_ifd0(tiff.clone(), TAIL);
        let big = tiff_in(true, true, 1);
        for (name, bytes) in [
            ("tiff gap before IFD0", moved),
            ("big-endian tiff", tailed(big.clone())),
        ] {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let out = run(&bytes, &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                let kept = stored(&out);
                assert!(!has(&kept, b"<html>"), "{at}: the payload stayed");
                assert_eq!(::image::load_from_memory(&kept).unwrap().width(), 2, "{at}");
                assert_eq!(has(&kept, b"2001:02:03"), u.with_metadata, "{at}: Exif");
                if u.with_metadata && name == "big-endian tiff" {
                    assert_eq!(kept, big, "{at}");
                }
            }
        }
    }

    /// Where a TIFF's regions end.
    fn tiff_end(bytes: &[u8]) -> Result<usize, TiffEnd> {
        tiff_regions(bytes).map(|r| r.iter().map(|x| x.1).max().unwrap())
    }

    /// Each end finder, directly: where it says a file ends, and when it says it can't.
    #[test]
    fn end_finders_say_where_files_end() {
        let bmp = encoded(&gradient(8, 8), Codec::Bmp);
        assert_eq!(bmp_end(&bmp), Some(bmp.len()));
        let with_size = |size: u32| {
            let mut b = tailed(bmp.clone());
            b[2..6].copy_from_slice(&size.to_le_bytes());
            b
        };
        assert_eq!(bmp_end(&with_size(0)), Some(bmp.len()), "from the pixels");
        assert_eq!(bmp_end(&with_size(53)), None, "shorter than the headers");
        assert_eq!(bmp_end(&with_size(10)), None);
        assert_eq!(bmp_end(&with_size(u32::MAX)), None, "past the end");
        let mut no_image_size = with_size(0);
        no_image_size[34..38].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(bmp_end(&no_image_size), Some(bmp.len()), "from the stride");

        let webp = encoded(&gradient(8, 8), Codec::WebP);
        assert_eq!(
            webp_end(&tailed(webp.clone())),
            Ok(Some((webp.len(), false)))
        );
        let mut unpadded = webp[..webp.len() - 1].to_vec();
        let len = (unpadded.len() - 8) as u32;
        unpadded[4..8].copy_from_slice(&len.to_le_bytes());
        assert_eq!(
            webp_end(&tailed(unpadded.clone())),
            Ok(Some((unpadded.len(), true)))
        );

        let avif = encoded(&gradient(8, 8), Codec::Avif);
        assert_eq!(
            avif::keep(&tailed(avif.clone()), true, 50_000_000).unwrap(),
            Some(avif.clone())
        );
        let boxes = top_boxes(&avif);
        let (mdat, _, _) = *boxes.last().unwrap();
        let mut to_eof = tailed(avif.clone());
        to_eof[mdat..mdat + 4].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            avif::keep(&to_eof, true, 50_000_000).unwrap(),
            Some(avif.clone()),
            "size 0: shortened to its items"
        );
        assert_eq!(
            avif::keep(&avif[..avif.len() - 1], true, 50_000_000).unwrap(),
            None,
            "an item past the end"
        );
        let no_meta: Vec<u8> = boxes
            .iter()
            .filter(|(s, _, _)| &avif[s + 4..s + 8] != b"meta")
            .flat_map(|&(s, _, e)| avif[s..e].to_vec())
            .collect();
        assert_eq!(
            avif::keep(&no_meta, true, 50_000_000).unwrap(),
            None,
            "no meta"
        );

        let tiff = tiff_with(true, 1);
        assert_eq!(tiff_end(&tailed(tiff.clone())), Ok(tiff.len()));
        assert_eq!(tiff_end(b"II+\0\x08\0\0\0"), Err(TiffEnd::Odd), "BigTIFF");
        let mut unknown = tiff.clone();
        let ifd0 = u32::from_le_bytes(unknown[4..8].try_into().unwrap()) as usize;
        unknown[ifd0 + 2 + 2..ifd0 + 2 + 4].copy_from_slice(&99u16.to_le_bytes());
        assert_eq!(
            tiff_end(&unknown),
            Err(TiffEnd::Odd),
            "a type it doesn't know"
        );
        assert_eq!(
            tiff_end(&tiff[..tiff.len() - 1]),
            Err(TiffEnd::Lies),
            "a value past the end"
        );
    }

    /// Payloads inside an icon's or an AVIF's declared bytes, ends that are awkward but
    /// valid, and a TIFF kept with every IFD it has.
    #[test]
    fn payloads_inside_the_file_go_too() {
        let png8 = encoded(&gradient(8, 8), Codec::Png);
        // An icon with a gap between its directory and its image.
        let gap = {
            let mut i = ico_around(&png8);
            let at = 6 + 16;
            i.splice(at..at, TAIL.iter().copied());
            let offset = (at + TAIL.len()) as u32;
            i[18..22].copy_from_slice(&offset.to_le_bytes());
            i
        };
        // A PNG entry whose declared size holds bytes after its IEND.
        let after_iend = {
            let mut p = png8.clone();
            p.extend_from_slice(TAIL);
            ico_around(&p)
        };
        // An entry whose size claims bytes after it.
        let size_claims = {
            let mut i = tailed(ico_around(&png8));
            let size = (png8.len() + TAIL.len()) as u32;
            i[14..18].copy_from_slice(&size.to_le_bytes());
            i
        };
        // A bitmap entry whose size holds bytes after its mask.
        let dib_claims = {
            let mut i = tailed(crate::fixtures::bitmap_icon());
            let size = u32::from_le_bytes(i[14..18].try_into().unwrap()) + TAIL.len() as u32;
            i[14..18].copy_from_slice(&size.to_le_bytes());
            i
        };
        let avif = encoded(&gradient(8, 8), Codec::Avif);
        let boxes = top_boxes(&avif);
        let (mdat_at, _, _) = *boxes.last().unwrap();
        assert_eq!(&avif[mdat_at + 4..mdat_at + 8], b"mdat");
        let with = |b: Vec<u8>| [avif.clone(), b].concat();
        let mdat_covers = {
            let mut a = tailed(avif.clone());
            let size = u32::from_be_bytes(a[mdat_at..mdat_at + 4].try_into().unwrap());
            let size = size + TAIL.len() as u32;
            a[mdat_at..mdat_at + 4].copy_from_slice(&size.to_be_bytes());
            a
        };
        // A payload inside `mdat`, ahead of its items, every item moved along past it.
        let mdat_payload = {
            let at = mdat_at + 8;
            let mut a = avif.clone();
            a.splice(at..at, TAIL.iter().copied());
            let size = u32::from_be_bytes(a[mdat_at..mdat_at + 4].try_into().unwrap());
            let size = size + TAIL.len() as u32;
            a[mdat_at..mdat_at + 4].copy_from_slice(&size.to_be_bytes());
            for (i, (_, off, len)) in crate::fixtures::avif_extents(&a).into_iter().enumerate() {
                if off >= at {
                    let moved = (off + TAIL.len()) as u32;
                    a = crate::fixtures::avif_set_extent(a, i, moved, len as u32);
                }
            }
            a
        };
        let mdat_to_eof = {
            let mut a = tailed(avif.clone());
            a[mdat_at..mdat_at + 4].copy_from_slice(&0u32.to_be_bytes());
            a
        };
        let interior: [(&str, Vec<u8>, u32); 10] = [
            ("avif payload in mdat before its items", mdat_payload, 8),
            ("ico gap before the image", gap, 8),
            ("ico PNG with bytes after IEND", after_iend, 8),
            ("ico size claiming the tail", size_claims, 8),
            ("ico bitmap size claiming the tail", dib_claims, 2),
            ("avif free box", with(isobmff_box(b"free", TAIL, false)), 8),
            (
                "avif unknown box",
                with(isobmff_box(b"zzzz", TAIL, false)),
                8,
            ),
            (
                "avif largesize box",
                with(isobmff_box(b"free", TAIL, true)),
                8,
            ),
            ("avif mdat covering the tail", mdat_covers, 8),
            ("avif mdat running to the end", mdat_to_eof, 8),
        ];
        for (name, bytes, side) in interior {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let out = run(&bytes, &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                let kept = stored(&out);
                assert!(!has(&kept, b"<html>"), "{at}: the payload stayed");
                assert!(!has(&kept, b"PK\x05\x06"), "{at}: the payload stayed");
                let d = dir();
                let again = file(&d, &kept);
                let p = probe(&again, &kept[..kept.len().min(8192)])
                    .unwrap_or_else(|e| panic!("{at}: stored but unreadable: {e}"));
                assert_eq!((p.width, p.height), (side, side), "{at}");
            }
        }
        // An AVIF's layout stays: the boxes it kept are where they were.
        let u = upload(serde_json::json!({"withMetadata": true}));
        let kept = stored(&run(&with(isobmff_box(b"free", TAIL, false)), &u).unwrap());
        assert_eq!(kept, avif, "the free box after mdat goes with the cut");

        // Valid, if unusual: a BMP whose file size field is 0, and a WebP whose last chunk
        // has an odd size and no pad byte. Both kept, cut where they end.
        let bmp = encoded(&gradient(8, 8), Codec::Bmp);
        let mut bmp0 = bmp.clone();
        bmp0[2..6].copy_from_slice(&0u32.to_le_bytes());
        // Its one chunk, the bitstream, is 145 bytes: the pad after it goes.
        let webp = encoded(&gradient(8, 8), Codec::WebP);
        assert_eq!(u32::from_le_bytes(webp[16..20].try_into().unwrap()) % 2, 1);
        let mut unpadded = webp[..webp.len() - 1].to_vec();
        let len = (unpadded.len() - 8) as u32;
        unpadded[4..8].copy_from_slice(&len.to_le_bytes());
        for (name, clean) in [
            ("bmp size 0", bmp0),
            ("webp without its last pad", unpadded),
        ] {
            for (how, u) in settings() {
                let at = format!("{name}, {how}");
                let out = run(&tailed(clean.clone()), &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                let kept = stored(&out);
                assert!(!has(&kept, b"<html>"), "{at}: the tail stayed");
                if u.with_metadata && name.starts_with("bmp") {
                    assert_eq!(kept, clean, "{at}");
                }
                assert_eq!(::image::load_from_memory(&kept).unwrap().width(), 8, "{at}");
            }
        }

        // A TIFF kept with withMetadata is cut, not re-encoded: its Exif and GPS IFDs and
        // every page stay byte for byte. Without it, it's re-encoded without them.
        for clean in [tiff_with(true, 1), tiff_with(false, 2)] {
            for (how, u) in settings() {
                let out = run(&tailed(clean.clone()), &u).unwrap_or_else(|e| panic!("{how}: {e}"));
                let kept = stored(&out);
                assert!(!has(&kept, b"<html>"), "{how}: the tail stayed");
                if u.with_metadata {
                    assert_eq!(kept, clean, "{how}");
                } else {
                    assert!(!has(&kept, b"2001:02:03"), "{how}: Exif stayed");
                }
            }
        }
        // An IFD pointing back at itself, and a strip past the end, lie about the end.
        let mut looped = tiff_with(true, 1);
        let ifd0 = u32::from_le_bytes(looped[4..8].try_into().unwrap()) as usize;
        let n = usize::from(u16::from_le_bytes([looped[ifd0], looped[ifd0 + 1]]));
        let next_at = ifd0 + 2 + 12 * n;
        let back = (ifd0 as u32).to_le_bytes();
        looped[next_at..next_at + 4].copy_from_slice(&back);
        let mut past = tiff_with(true, 1);
        let strip = (0..n)
            .map(|e| ifd0 + 2 + 12 * e)
            .find(|&at| u16::from_le_bytes([past[at], past[at + 1]]) == 273)
            .unwrap();
        past[strip + 8..strip + 12].copy_from_slice(&1_000_000u32.to_le_bytes());
        for (name, bytes) in [("tiff IFD loop", looped), ("tiff strip past the end", past)] {
            let meta = upload(serde_json::json!({"withMetadata": true}));
            assert!(run(&tailed(bytes), &meta).is_err(), "{name}: stored");
        }
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
    fn a_crop_cuts_the_picture_as_shown() {
        let d = dir();
        let u = upload(serde_json::json!({}));
        let rgb = RgbImage::from_fn(1000, 800, |x, y| {
            ::image::Rgb([
                if x < 500 { 255 } else { 0 },
                if y < 400 { 255 } else { 0 },
                0,
            ])
        });
        let png = encoded(&DynamicImage::ImageRgb8(rgb), Codec::Png);
        let input = file(&d, &png);
        let p = probe(&input, &png).unwrap();
        let out = crop(&input, &p, (500, 400, 400, 300), &u, 50_000_000, &d).unwrap();
        assert_eq!((out.width, out.height, out.mime), (400, 300, "image/png"));
        assert!(out.blurhash.is_some());
        let made = ::image::load_from_memory(&std::fs::read(&out.path).unwrap())
            .unwrap()
            .to_rgb8();
        assert_eq!(made.dimensions(), (400, 300));
        assert_eq!(
            made.get_pixel(0, 0).0,
            [0, 0, 0],
            "the bottom-right quarter"
        );
        // Past the edge, or empty: refused, nothing written.
        for rect in [(700, 0, 301, 10), (0, 0, 0, 10), (0, 800, 1, 1)] {
            assert!(matches!(
                crop(&input, &p, rect, &u, 50_000_000, &d),
                Err(ImageError::Corrupt(_))
            ));
        }
        // Held to the cap like any decode.
        let small = upload(serde_json::json!({"constructorOptions": {"limitInputPixels": 1000}}));
        assert!(matches!(
            crop(&input, &p, (0, 0, 10, 10), &small, 50_000_000, &d),
            Err(ImageError::TooManyPixels { .. })
        ));
        // A turned JPEG keeping its metadata is cut where it's shown: 200 across, 400 down.
        let jpeg = with_exif(
            encoded(&gradient(400, 200), Codec::Jpeg),
            exif(Some(6), true),
        );
        let input = file(&d, &jpeg);
        let p = probe(&input, &jpeg).unwrap();
        let keep = upload(serde_json::json!({"withMetadata": true}));
        let out = crop(&input, &p, (0, 0, 200, 100), &keep, 50_000_000, &d).unwrap();
        assert_eq!((out.width, out.height), (200, 100));
        let stored = ::image::load_from_memory(&std::fs::read(&out.path).unwrap()).unwrap();
        assert_eq!(
            (stored.width(), stored.height()),
            (200, 100),
            "upright, orientation 1"
        );
        // Every frame of an animation is cut.
        let g = moving_gif(10, 10, 3);
        let input = file(&d, &g);
        let p = probe(&input, &g).unwrap();
        let out = crop(&input, &p, (2, 2, 5, 4), &u, 50_000_000, &d).unwrap();
        assert_eq!((out.width, out.height, out.mime), (5, 4, "image/gif"));
        let again = probe(&out.path, &std::fs::read(&out.path).unwrap()).unwrap();
        assert_eq!((again.width, again.height, again.frames), (5, 4, 3));
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
        // A PNG cut inside a chunk has no end to cut a tail at and doesn't decode, so
        // there's nothing it could be re-encoded from.
        assert_eq!(run(cut, &lenient), Err(ImageError::Truncated));
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
        // `failOn: none`, so its end (which can't be found) sends it to the decoder rather
        // than refusing it before.
        let lenient = json(serde_json::json!({"constructorOptions": {"failOn": "none"}}));
        let got = process_original(&input, &fake, &lenient, 1000, &d);
        assert!(
            matches!(got, Err(ImageError::Corrupt(_) | ImageError::Truncated)),
            "{got:?}"
        );
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

    /// A suggested palette and a histogram describe a picture without being needed to show
    /// it, and a palette may repeat: they go with the rest of the metadata, in an icon too.
    #[test]
    fn png_palettes_and_histograms_go() {
        let mut png =
            img_parts::png::Png::from_bytes(encoded(&gradient(4, 4), Codec::Png).into()).unwrap();
        let at = png.chunks().len() - 1;
        let mut splt = b"pal\0\x08".to_vec();
        splt.extend_from_slice(TAIL);
        for (kind, body) in [
            (b"sPLT", &splt),
            (b"sPLT", &splt),
            (b"hIST", &TAIL.to_vec()),
        ] {
            png.chunks_mut().insert(
                at,
                img_parts::png::PngChunk::new(*kind, body.clone().into()),
            );
        }
        let png = png.encoder().bytes().to_vec();
        assert!(has(&png, b"<html>"));
        for (name, bytes) in [("png", png.clone()), ("icon", ico_around(&png))] {
            let out = run(&bytes, &upload(serde_json::json!({}))).unwrap();
            let kept = stored(&out);
            assert!(!has(&kept, b"<html>"), "{name}");
            assert!(!has(&kept, b"sPLT") && !has(&kept, b"hIST"), "{name}");
        }
    }

    /// A colour profile is kept only when it's shaped like one: its declared size its
    /// length, `acsp` at 36. Anything else could be anything, so it goes.
    #[test]
    fn a_colour_profile_that_isnt_one_goes() {
        let good = crate::fixtures::fake_icc(600);
        let mut sized = good.clone();
        sized[100..100 + TAIL.len()].copy_from_slice(TAIL);
        sized[..4].copy_from_slice(&599u32.to_be_bytes());
        let mut unsigned = good.clone();
        unsigned[100..100 + TAIL.len()].copy_from_slice(TAIL);
        unsigned[36..40].copy_from_slice(b"none");
        let profile = |kept: &[u8], mime: &str| -> Option<Vec<u8>> {
            match mime {
                "image/avif" => avif::meta(kept)
                    .unwrap()
                    .icc
                    .map(|(s, e)| kept[s..e].to_vec()),
                _ => img_parts::DynImage::from_bytes(kept.to_vec().into())
                    .unwrap()
                    .unwrap()
                    .icc_profile()
                    .map(|b| b.to_vec()),
            }
        };
        let gradient = gradient(8, 8);
        // JPEG's profile is split across APP2 segments: over 64 KiB makes two.
        let big = crate::fixtures::fake_icc(70_000);
        let as_png = || encoded(&gradient, Codec::Png);
        for (name, icc, bytes) in [
            ("jpeg", &good, encoded(&gradient, Codec::Jpeg)),
            ("jpeg in two", &big, encoded(&gradient, Codec::Jpeg)),
            ("png", &good, as_png()),
            ("webp", &good, encoded(&gradient, Codec::WebP)),
        ]
        .into_iter()
        .map(|(n, i, b)| (n, i, crate::fixtures::with_icc(b, i)))
        .chain([(
            "avif",
            &good,
            avif::with_icc(&encoded(&gradient, Codec::Avif), &good).unwrap(),
        )]) {
            let out = run(&bytes, &upload(serde_json::json!({}))).unwrap();
            assert_eq!(
                profile(&stored(&out), out.mime).as_ref(),
                Some(icc),
                "{name}: kept"
            );
        }
        for (shape, bad) in [("size", &sized), ("signature", &unsigned)] {
            for (name, bytes) in [
                (
                    "jpeg",
                    crate::fixtures::with_icc(encoded(&gradient, Codec::Jpeg), bad),
                ),
                ("png", crate::fixtures::with_icc(as_png(), bad)),
                (
                    "webp",
                    crate::fixtures::with_icc(encoded(&gradient, Codec::WebP), bad),
                ),
                (
                    "avif",
                    avif::with_icc(&encoded(&gradient, Codec::Avif), bad).unwrap(),
                ),
            ] {
                let at = format!("{name}, {shape}");
                for u in [
                    upload(serde_json::json!({})),
                    upload(serde_json::json!({"constructorOptions": {"failOn": "none"}})),
                    // Re-encoded, it isn't carried over either.
                    upload(serde_json::json!({"formatOptions": {"format": "png"}})),
                    upload(serde_json::json!({"formatOptions": {"format": "avif"}})),
                ] {
                    let out = run(&bytes, &u).unwrap_or_else(|e| panic!("{at}: {e}"));
                    let kept = stored(&out);
                    assert!(!has(&kept, b"<html>"), "{at}: the profile stayed");
                    assert_eq!(profile(&kept, out.mime), None, "{at}");
                    assert_eq!(pixels_of(&kept).len(), 8 * 8 * 4, "{at}");
                }
            }
        }
    }

    /// A 2×2 24-bit BMP with a version 5 header and `profile` embedded after its pixels.
    fn bmp_v5(profile: &[u8]) -> Vec<u8> {
        let pixels = [
            [10u8, 80, 160, 240, 30, 60, 0, 0],
            [90, 20, 200, 5, 70, 140, 0, 0],
        ]
        .concat();
        let start = 14 + 124 + pixels.len();
        let le = |v: u32| v.to_le_bytes();
        let mut out = b"BM".to_vec();
        out.extend(le((start + profile.len()) as u32));
        out.extend([0; 4]);
        out.extend(le(14 + 124));
        let mut info = vec![0u8; 124];
        info[..4].copy_from_slice(&le(124));
        info[4..8].copy_from_slice(&le(2));
        info[8..12].copy_from_slice(&le(2));
        info[12..14].copy_from_slice(&1u16.to_le_bytes());
        info[14..16].copy_from_slice(&24u16.to_le_bytes());
        info[20..24].copy_from_slice(&le(pixels.len() as u32));
        info[56..60].copy_from_slice(b"DEBM"); // PROFILE_EMBEDDED, 'MBED' read little-endian
        info[112..116].copy_from_slice(&le((124 + pixels.len()) as u32));
        info[116..120].copy_from_slice(&le(profile.len() as u32));
        out.extend(info);
        out.extend(pixels);
        out.extend_from_slice(profile);
        out
    }

    /// A version 5 BMP's embedded profile is kept only when it's shaped like one.
    #[test]
    fn a_bmp_profile_that_isnt_one_goes() {
        let good = crate::fixtures::fake_icc(600);
        let mut bad = good.clone();
        bad[100..100 + TAIL.len()].copy_from_slice(TAIL);
        bad[36..40].copy_from_slice(b"none");
        let want = pixels_of(&bmp_v5(&good));
        for (how, u) in settings() {
            let kept = stored(&run(&bmp_v5(&good), &u).unwrap());
            assert!(
                kept.windows(good.len()).any(|w| w == good),
                "{how}: the profile went"
            );
            let out = run(&bmp_v5(&bad), &u).unwrap_or_else(|e| panic!("{how}: {e}"));
            let kept = stored(&out);
            assert!(!has(&kept, b"<html>"), "{how}: the profile stayed");
            assert_eq!(pixels_of(&kept), want, "{how}");
        }
    }

    #[test]
    fn each_encoder_s_limit_is_exact() {
        for (format, name, most) in [
            (InputFormat::Jpeg, "a JPEG", 65_535),
            (InputFormat::Gif, "a GIF", 65_535),
            (InputFormat::Avif, "an AVIF", 65_535),
            (InputFormat::Webp, "a WebP", 16_383),
            (InputFormat::Ico, "an icon", 256),
        ] {
            // The limit is the format made's, not the image's: it read fine.
            let refused = Err(format!(
                "this image can't be made into {name}: it's {} pixels across; the limit is {most}",
                most + 1
            ));
            let fits = |w, h| fits(w, h, format).map_err(|e| e.to_string());
            assert_eq!(fits(most, 1), Ok(()), "{name}");
            assert_eq!(fits(1, most), Ok(()), "{name}, tall");
            assert_eq!(fits(most + 1, 1), refused, "{name}");
            assert_eq!(fits(1, most + 1), refused, "{name}, tall");
        }
        for format in [InputFormat::Png, InputFormat::Tiff, InputFormat::Bmp] {
            assert_eq!(fits(u32::MAX, u32::MAX, format), Ok(()));
        }
    }

    #[test]
    fn a_picture_wider_than_its_encoder_takes_is_refused() {
        let png = |w, h| encoded(&gradient(w, h), Codec::Png);
        let refused = |name: &'static str, v: u32, most: u32| {
            Err(ImageError::TooWide {
                made: name,
                side: v,
                most,
            })
        };
        for (format, name, most) in [
            ("webp", "a WebP", 16_383),
            ("gif", "a GIF", 65_535),
            ("jpeg", "a JPEG", 65_535),
        ] {
            let u = upload(serde_json::json!({"formatOptions": {"format": format}}));
            let got = run(&png(most + 1, 2), &u).map(|p| p.width);
            assert_eq!(got, refused(name, most + 1, most), "{format}");
            let got = run(&png(2, most + 1), &u).map(|p| p.width);
            assert_eq!(got, refused(name, most + 1, most), "{format}, tall");
            assert_eq!(
                run(&png(most, 2), &u).map(|p| p.width),
                Ok(most),
                "{format}"
            );
        }
        // AVIF's limit is rav1e's. Storing one at it takes minutes in a debug build, so
        // `each_encoder_s_limit_is_exact` checks the edge.
        let u = upload(serde_json::json!({"formatOptions": {"format": "avif"}}));
        let got = run(&png(65_536, 2), &u).map(|p| p.width);
        assert_eq!(got, refused("an AVIF", 65_536, 65_535));
        // An animation goes through its own encoder.
        let u = upload(serde_json::json!({"formatOptions": {"format": "webp"}}));
        let got = run(&moving_gif(16_384, 1, 2), &u).map(|p| p.width);
        assert_eq!(got, refused("a WebP", 16_384, 16_383));
        // An icon's entry wider than its directory says, made again.
        let ico = ico_around(&png(280, 2));
        let u = upload(serde_json::json!({"resizeOptions": {"width": 270}}));
        assert_eq!(run(&ico, &u).map(|p| p.width), refused("an icon", 270, 256));
    }

    #[test]
    fn a_big_avif_is_encoded_fast() {
        // Effort 4 (speed 6) on 50 MP took 61 s in release on 12 cores; speed 9, 27 s.
        assert_eq!(avif_speed(None, 16_000_000), 6);
        assert_eq!(avif_speed(Some(9), 16_000_000), 1);
        assert_eq!(avif_speed(None, 16_000_001), 9);
        assert_eq!(avif_speed(Some(9), 50_000_000), 9);
        assert_eq!(avif_speed(Some(0), 50_000_000), 10);
    }
}
