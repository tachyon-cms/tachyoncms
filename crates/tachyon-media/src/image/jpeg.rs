//! A JPEG's markers, walked through every scan, so metadata between or after scans is
//! seen and what follows the end-of-image marker is known.

use super::ImageError;

/// A JPEG without its metadata, and whether its end-of-image marker was found.
pub(super) struct Walked {
    pub stripped: Vec<u8>,
    pub ends: bool,
}

/// Walks `bytes`, keeping every segment decoding needs (JFIF, an ICC profile, Adobe's
/// colour transform, tables, frames, scans) and dropping the other APPn segments and
/// comments wherever they are. Stops at the first end-of-image marker; what follows it
/// (more pictures, a video, a vendor trailer) goes.
pub(super) fn walk(bytes: &[u8]) -> Result<Walked, ImageError> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return Err(ImageError::Corrupt("it doesn't start like a JPEG".into()));
    }
    let mut out = vec![0xFF, 0xD8];
    let mut i = 2;
    loop {
        // Fill bytes may come before a marker.
        while bytes.get(i) == Some(&0xFF) && bytes.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        let (Some(&0xFF), Some(&marker)) = (bytes.get(i), bytes.get(i + 1)) else {
            return Ok(Walked {
                stripped: out,
                ends: false,
            });
        };
        match marker {
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Ok(Walked {
                    stripped: out,
                    ends: true,
                });
            }
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&[0xFF, marker]);
                i += 2;
                continue;
            }
            _ => {}
        }
        let Some(len) = bytes.get(i + 2..i + 4) else {
            return Ok(Walked {
                stripped: out,
                ends: false,
            });
        };
        let end = i + 2 + usize::from(u16::from_be_bytes([len[0], len[1]]));
        let Some(segment) = bytes.get(i..end) else {
            return Ok(Walked {
                stripped: out,
                ends: false,
            });
        };
        let body = &segment[4.min(segment.len())..];
        let keep = match marker {
            0xE0 => body.starts_with(b"JFIF\0"),
            0xE2 => body.starts_with(b"ICC_PROFILE\0"),
            0xEE => body.starts_with(b"Adobe"),
            0xE1..=0xEF | 0xFE => false,
            _ => true,
        };
        if keep {
            out.extend_from_slice(segment);
        }
        i = end;
        if marker == 0xDA {
            // Entropy-coded data: `FF 00` and restart markers are part of it.
            let start = i;
            while let Some(&b) = bytes.get(i) {
                if b == 0xFF {
                    match bytes.get(i + 1) {
                        Some(0x00 | 0xD0..=0xD7) => i += 2,
                        Some(_) | None => break,
                    }
                } else {
                    i += 1;
                }
            }
            out.extend_from_slice(&bytes[start..i.min(bytes.len())]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, marker];
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn markers_in_and_after_scans_are_seen() {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(segment(0xE0, b"JFIF\0rest"));
        jpeg.extend(segment(0xDA, b"\x01\x01\x00\x00\x3F\x00"));
        jpeg.extend_from_slice(&[1, 2, 0xFF, 0x00, 3, 0xFF, 0xD0, 4]);
        jpeg.extend(segment(0xE1, b"Exif\0\0GPSLatitude=51.5N"));
        jpeg.extend(segment(0xFE, b"secret!"));
        jpeg.extend(segment(0xDA, b"\x01\x01\x00\x00\x3F\x00"));
        jpeg.extend_from_slice(&[5, 6]);
        jpeg.extend(segment(0xED, b"Photoshop 3.0\0\xFF\xD9 iptc"));
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg.extend_from_slice(b"ftypmp4 a video after the picture");
        let w = walk(&jpeg).unwrap();
        assert!(w.ends);
        let text = String::from_utf8_lossy(&w.stripped);
        for gone in ["GPS", "secret", "iptc", "video"] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
        assert!(w.stripped.ends_with(&[5, 6, 0xFF, 0xD9]));
        assert!(text.contains("JFIF"));
    }

    #[test]
    fn a_cut_file_has_no_end() {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(segment(0xDA, b"\x01\x01\x00\x00\x3F\x00"));
        jpeg.extend_from_slice(&[1, 2, 3]);
        assert!(!walk(&jpeg).unwrap().ends);
        assert!(!walk(&jpeg[..5]).unwrap().ends);
    }
}
