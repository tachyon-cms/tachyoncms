//! The images made from a stored original for a collection's `imageSizes`, such as a
//! `thumbnail`, each with an AVIF and a WebP copy.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tachyon_core::uploads::{EncodeOptions, Fit, ImageFormat, ImageSize, size_filename};

use super::{
    ImageError, InputFormat, Meta, Shown, check_cap, decode, encode_animation, encode_to, internal,
    made, mime_of, probe_unguarded, resize_around, unpanicked, upright_exif, write_new,
};

/// A size made: its `sizes.<name>` entry (`width`, `height`, `mimeType`, `filesize`,
/// `filename`), its copies in other formats by `formats` key, and the files to store, by
/// name. The files are deleted when this drops, so store them first.
#[derive(Debug)]
pub struct SizeOut {
    pub entry: Map<String, Value>,
    pub formats: Vec<(&'static str, Map<String, Value>)>,
    pub files: Vec<(String, PathBuf)>,
}

impl Drop for SizeOut {
    fn drop(&mut self) {
        for (_, path) in &self.files {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The format a size of an original of type `mime` is made in when it names none: the
/// original's own, PNG for a TIFF, BMP or icon. `None` for anything else, which gets no
/// sizes.
pub fn size_format(mime: &str) -> Option<ImageFormat> {
    Some(match mime {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" | "image/tiff" | "image/bmp" | "image/vnd.microsoft.icon" | "image/x-icon" => {
            ImageFormat::Png
        }
        "image/webp" => ImageFormat::Webp,
        "image/avif" => ImageFormat::Avif,
        "image/gif" => ImageFormat::Gif,
        _ => return None,
    })
}

fn extension(f: InputFormat) -> &'static str {
    match f {
        InputFormat::Jpeg => "jpg",
        InputFormat::Png => "png",
        InputFormat::Webp => "webp",
        InputFormat::Avif => "avif",
        InputFormat::Gif => "gif",
        InputFormat::Tiff => "tiff",
        InputFormat::Bmp => "bmp",
        InputFormat::Ico => "ico",
    }
}

/// A stored original decoded once, upright, every frame of an animation kept, for making
/// each of its sizes from.
pub struct Original {
    frames: Vec<Shown>,
    loops: Option<u16>,
    meta: Meta,
}

/// What making one size came to.
#[derive(Debug)]
pub enum Made {
    Size(SizeOut),
    /// Larger than the original in both dimensions, and `withoutEnlargement` isn't false.
    Skipped,
    /// It can't be made from this original: over the pixel cap, or the resize or encode
    /// failed. Making it again would fail the same way.
    Failed(ImageError),
}

impl Original {
    /// The image at `path` decoded with at most `cap` pixels, all frames counted. The
    /// colour profile is kept, and with `keep_metadata` (`withMetadata`) its EXIF, its
    /// orientation reset since the frames are turned upright, and XMP. A file that can't be
    /// read is `Internal`; one that can't be decoded is any other error.
    pub fn open(path: &Path, cap: u64, keep_metadata: bool) -> Result<Original, ImageError> {
        let bytes = std::fs::read(path).map_err(internal)?;
        unpanicked(|| {
            let probe = probe_unguarded(path, &bytes[..bytes.len().min(8192)])?;
            check_cap(&probe, cap)?;
            let d = decode(&bytes, &probe, cap, probe.frames > 1, keep_metadata)?;
            Ok(Original {
                frames: d.frames,
                loops: d.loops,
                meta: Meta {
                    icc: d.meta.icc,
                    exif: d.meta.exif.map(|e| upright_exif(&e)),
                    xmp: d.meta.xmp,
                },
            })
        })
    }

    /// `size` made from this original, the stored file named `filename`, into new files in
    /// `out_dir`, holding the result to `cap` pixels, all frames counted. The error is only
    /// for files that couldn't be written (`Internal`): anything else is `Made::Failed`.
    ///
    /// One side only keeps the aspect ratio; both are fitted as `fit` says, a cover crop
    /// centred on `focal` (percent across and down; given when `focalPoint` is on) unless
    /// the size sets a `position` or `crop`. It's made in its `formatOptions`, else
    /// `default_format`, with AVIF and WebP copies besides unless it's already one of
    /// those. Every frame of an animation is resized; a GIF or WebP keeps them all, an
    /// AVIF the first.
    pub fn make(
        &self,
        filename: &str,
        size: &ImageSize,
        focal: Option<(f32, f32)>,
        default_format: ImageFormat,
        cap: u64,
        out_dir: &Path,
    ) -> Result<Made, ImageError> {
        let encoded = match unpanicked(|| self.encode(size, focal, default_format, cap)) {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(Made::Skipped),
            Err(e) => return Ok(Made::Failed(e)),
        };
        let mut out = SizeOut {
            entry: Map::new(),
            formats: Vec::new(),
            files: Vec::new(),
        };
        for (key, format, w, h, bytes) in encoded {
            // Should this fail, `out` drops and takes the files written so far with it.
            let (path, filesize) = write_new(out_dir, &bytes)?;
            let name = size_filename(filename, w, h, extension(format));
            out.files.push((name.clone(), path));
            let mut entry = Map::new();
            entry.insert("width".into(), w.into());
            entry.insert("height".into(), h.into());
            entry.insert("mimeType".into(), mime_of(format).into());
            entry.insert("filesize".into(), filesize.into());
            entry.insert("filename".into(), name.into());
            match key {
                None => out.entry = entry,
                Some(k) => out.formats.push((k, entry)),
            }
        }
        Ok(Made::Size(out))
    }

    /// Each output of `size` (the size itself, then its copies by `formats` key) as its
    /// format, width, height and bytes; `None` when it's skipped.
    #[allow(clippy::type_complexity)]
    fn encode(
        &self,
        size: &ImageSize,
        focal: Option<(f32, f32)>,
        default_format: ImageFormat,
        cap: u64,
    ) -> Result<Option<Vec<(Option<&'static str>, InputFormat, u32, u32, Vec<u8>)>>, ImageError>
    {
        let (iw, ih) = (self.frames[0].image.width(), self.frames[0].image.height());
        let aspect = |a: u32, b: u32, c: u32| (f64::from(a) * f64::from(b) / f64::from(c)).round();
        let (tw, th) = match (size.width, size.height) {
            (Some(w), Some(h)) => (f64::from(w), f64::from(h)),
            (Some(w), None) => (f64::from(w), aspect(ih, w, iw)),
            (None, Some(h)) => (aspect(iw, h, ih), f64::from(h)),
            (None, None) => (f64::from(iw), f64::from(ih)),
        };
        if size.without_enlargement.unwrap_or(true) && tw > f64::from(iw) && th > f64::from(ih) {
            return Ok(None);
        }
        let position = size.position.or(size.crop);
        let both = size.width.is_some() && size.height.is_some();
        let focal = focal
            .filter(|_| both && size.fit == Fit::Cover && position.is_none())
            .map(|(x, y)| {
                let at = |v: f32| (f64::from(v) / 100.0).clamp(0.0, 1.0);
                (at(x), at(y))
            });
        let count = self.frames.len() as u64;
        let mut frames = Vec::with_capacity(self.frames.len());
        for (i, f) in self.frames.iter().enumerate() {
            let image = resize_around(
                &f.image,
                size.width,
                size.height,
                size.fit,
                position.unwrap_or_default(),
                focal,
                // Unset, a size the check above let through may grow, as sharp's default
                // does.
                size.without_enlargement == Some(true),
                cap,
            )
            // Resizing fails the same way however often it's tried.
            .map_err(|e| ImageError::Corrupt(e.to_string()))?;
            if i == 0 {
                let total = u64::from(image.width()) * u64::from(image.height());
                if total.saturating_mul(count) > cap {
                    return Err(ImageError::Corrupt(format!(
                        "the size would be {total} pixels a frame, {count} frames; the limit is {cap}"
                    )));
                }
            }
            frames.push(Shown {
                image,
                delay_ms: f.delay_ms,
            });
        }
        let (main, options) = match &size.format_options {
            Some(f) => (made(f.format), f.options.clone()),
            None => (made(default_format), EncodeOptions::default()),
        };
        let (w, h) = (frames[0].image.width(), frames[0].image.height());
        let plain = EncodeOptions::default();
        let outputs = [
            (None, main, &options),
            (Some("avif"), InputFormat::Avif, &plain),
            (Some("webp"), InputFormat::Webp, &plain),
        ];
        let mut out = Vec::new();
        for (key, format, o) in outputs {
            if key.is_some() && format == main {
                continue;
            }
            let bytes = match format {
                InputFormat::Gif | InputFormat::Webp if frames.len() > 1 => {
                    encode_animation(frames.clone(), self.loops, format, o, &self.meta)?
                }
                _ => encode_to(&frames[0].image, format, o, &self.meta)?,
            };
            out.push((key, format, w, h, bytes));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{encoded, gradient, moving_gif};
    use ::image::{DynamicImage, ImageFormat as Codec, Rgb, RgbImage};
    use serde_json::json;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tachyon-sizes-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn size(v: Value) -> ImageSize {
        serde_json::from_value(v).unwrap()
    }

    /// `bytes` written to `d`, and a size of it as `size` says, from `a.png`.
    fn sized(
        bytes: &[u8],
        s: Value,
        focal: Option<(f32, f32)>,
        format: ImageFormat,
    ) -> Option<SizeOut> {
        let d = dir();
        let input = d.join("in");
        std::fs::write(&input, bytes).unwrap();
        let original = Original::open(&input, 50_000_000, false).unwrap();
        match original
            .make("a.png", &size(s), focal, format, 50_000_000, &d)
            .unwrap()
        {
            Made::Size(out) => Some(out),
            Made::Skipped => None,
            Made::Failed(e) => panic!("{e}"),
        }
    }

    fn halves() -> Vec<u8> {
        let img = RgbImage::from_fn(1000, 500, |x, _| {
            if x < 500 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        });
        encoded(&DynamicImage::ImageRgb8(img), Codec::Png)
    }

    fn centre(out: &SizeOut) -> [u8; 3] {
        let img = ::image::load_from_memory(&std::fs::read(&out.files[0].1).unwrap())
            .unwrap()
            .to_rgb8();
        img.get_pixel(img.width() / 2, img.height() / 2).0
    }

    #[test]
    fn a_cover_crop_keeps_the_focal_point() {
        let png = halves();
        let s = json!({"name": "square", "width": 200, "height": 200});
        let out = sized(&png, s.clone(), Some((10.0, 50.0)), ImageFormat::Png).unwrap();
        assert_eq!(out.entry["width"], 200);
        let [r, _, b] = centre(&out);
        assert!(r > 200 && b < 50, "red: {r} {b}");
        let out = sized(&png, s.clone(), Some((90.0, 50.0)), ImageFormat::Png).unwrap();
        let [r, _, b] = centre(&out);
        assert!(b > 200 && r < 50, "blue: {r} {b}");
        // A position the size sets wins over the focal point.
        let mut at = s;
        at["position"] = json!("right");
        let out = sized(&png, at, Some((10.0, 50.0)), ImageFormat::Png).unwrap();
        let [r, _, b] = centre(&out);
        assert!(b > 200 && r < 50, "the position: {r} {b}");
    }

    #[test]
    fn one_side_keeps_the_aspect_ratio_and_copies_are_made() {
        let png = encoded(&gradient(1000, 500), Codec::Png);
        let out = sized(
            &png,
            json!({"name": "card", "width": 400}),
            None,
            ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(
            Value::Object(out.entry.clone()),
            json!({"width": 400, "height": 200, "mimeType": "image/png",
                   "filesize": std::fs::metadata(&out.files[0].1).unwrap().len(),
                   "filename": "a-400x200.png"})
        );
        let keys: Vec<_> = out.formats.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["avif", "webp"]);
        assert_eq!(out.formats[0].1["mimeType"], "image/avif");
        assert_eq!(out.formats[0].1["filename"], "a-400x200.avif");
        assert_eq!(out.formats[1].1["filename"], "a-400x200.webp");
        let names: Vec<_> = out.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a-400x200.png", "a-400x200.avif", "a-400x200.webp"]);
        let avif = std::fs::read(&out.files[1].1).unwrap();
        let p = super::super::probe(&out.files[1].1, &avif[..avif.len().min(8192)]).unwrap();
        assert_eq!((p.format, p.width), (InputFormat::Avif, 400), "an AVIF");
        let paths: Vec<PathBuf> = out.files.iter().map(|(_, p)| p.clone()).collect();
        drop(out);
        assert!(paths.iter().all(|p| !p.exists()), "deleted on drop");
    }

    #[test]
    fn a_size_already_in_a_copy_format_has_no_second_copy() {
        let png = encoded(&gradient(100, 100), Codec::Png);
        let out = sized(
            &png,
            json!({"name": "w", "width": 50,
                   "formatOptions": {"format": "webp", "options": {"quality": 60}}}),
            None,
            ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(out.entry["mimeType"], "image/webp");
        assert_eq!(out.entry["filename"], "a-50x50.webp");
        let keys: Vec<_> = out.formats.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["avif"]);
    }

    #[test]
    fn small_images_skip_sizes_unless_enlarging_is_allowed() {
        let png = encoded(&gradient(100, 100), Codec::Png);
        for s in [
            json!({"name": "big", "width": 400, "height": 400}),
            json!({"name": "big", "width": 400}),
            json!({"name": "big", "width": 400, "height": 400, "withoutEnlargement": true}),
        ] {
            assert!(
                sized(&png, s.clone(), None, ImageFormat::Png).is_none(),
                "{s}"
            );
        }
        let out = sized(
            &png,
            json!({"name": "big", "width": 400, "height": 400, "withoutEnlargement": false}),
            None,
            ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(
            (&out.entry["width"], &out.entry["height"]),
            (&json!(400), &json!(400))
        );
        // Larger one way only, it's made.
        let out = sized(
            &png,
            json!({"name": "wide", "width": 400, "height": 50}),
            None,
            ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(
            (&out.entry["width"], &out.entry["height"]),
            (&json!(400), &json!(50))
        );
    }

    #[test]
    fn every_frame_of_an_animation_is_resized() {
        let gif = moving_gif(40, 20, 3);
        let out = sized(
            &gif,
            json!({"name": "small", "width": 20}),
            None,
            ImageFormat::Gif,
        )
        .unwrap();
        assert_eq!(out.entry["mimeType"], "image/gif");
        assert_eq!(out.entry["height"], 10);
        let bytes = std::fs::read(&out.files[0].1).unwrap();
        let mut d = ::gif::DecodeOptions::new()
            .read_info(std::io::Cursor::new(&bytes))
            .unwrap();
        let mut frames = 0;
        while d.read_next_frame().unwrap().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 3);
        assert_eq!(size_format("image/gif"), Some(ImageFormat::Gif));
        assert_eq!(size_format("image/tiff"), Some(ImageFormat::Png));
        assert_eq!(size_format("image/svg+xml"), None);
    }

    #[test]
    fn one_decode_makes_every_size_and_a_bad_one_fails_alone() {
        let d = dir();
        let input = d.join("in");
        std::fs::write(&input, encoded(&gradient(100, 5000), Codec::Png)).unwrap();
        let original = Original::open(&input, 50_000_000, false).unwrap();
        let wide = size(json!({"name": "wide", "width": 1920, "withoutEnlargement": false}));
        let made = original
            .make("a.png", &wide, None, ImageFormat::Png, 50_000_000, &d)
            .unwrap();
        assert!(
            matches!(made, Made::Failed(ImageError::Corrupt(_))),
            "{made:?}"
        );
        let thumb = size(json!({"name": "thumb", "width": 10}));
        let made = original
            .make("a.png", &thumb, None, ImageFormat::Png, 50_000_000, &d)
            .unwrap();
        let Made::Size(out) = made else {
            panic!("{made:?}")
        };
        assert_eq!(out.entry["height"], 500);
    }

    #[test]
    fn with_metadata_sizes_keep_exif_and_xmp() {
        use crate::fixtures::{exif, with_exif};
        use img_parts::ImageEXIF;
        let jpeg = with_exif(
            encoded(&gradient(200, 100), Codec::Jpeg),
            exif(Some(1), true),
        );
        let d = dir();
        let input = d.join("in");
        std::fs::write(&input, &jpeg).unwrap();
        let s = size(json!({"name": "thumb", "width": 50}));
        let exif_of = |keep: bool| {
            let original = Original::open(&input, 50_000_000, keep).unwrap();
            let Made::Size(out) = original
                .make("a.jpg", &s, None, ImageFormat::Jpeg, 50_000_000, &d)
                .unwrap()
            else {
                panic!("not made")
            };
            let bytes = std::fs::read(&out.files[0].1).unwrap();
            img_parts::jpeg::Jpeg::from_bytes(bytes.into())
                .unwrap()
                .exif()
        };
        assert!(exif_of(true).is_some(), "kept with withMetadata");
        assert!(exif_of(false).is_none(), "stripped without it");
    }
}
