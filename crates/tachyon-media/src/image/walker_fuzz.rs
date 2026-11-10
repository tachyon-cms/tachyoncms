//! Seeded mutation fuzz for the container walkers: valid files with their lengths,
//! offsets and bytes broken at random. No walker may panic or take long, and whatever one
//! strips must walk again with nothing left to strip. Structure-aware cases set the
//! declared sizes and offsets to their edges and run the real probe as well.

use std::time::{Duration, Instant};

use super::{avif, gif, ico, jpeg, probe};
use crate::fixtures::{
    avif_extents, avif_set_extent, avif_with_exif, encoded, exif, gradient, iccp_bomb_png,
    ico_around, ico_of, moving_gif, rgba, with_exif,
};
use ::image::ImageFormat as Codec;

/// xorshift64: the same cases every run.
struct Rng(u64);

impl Rng {
    fn n(&mut self, m: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % m.max(1) as u64) as usize
    }
}

/// `seed` broken in one to four places.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    for _ in 0..1 + rng.n(4) {
        if b.is_empty() {
            break;
        }
        let at = rng.n(b.len());
        match rng.n(7) {
            0 => b[at] ^= 1 << rng.n(8),
            1 => b[at] = [0x00, 0xFF, 0x7F, 0x80][rng.n(4)],
            // A length or offset made huge, tiny or off by one.
            2 => {
                let v: u32 = [0, 1, 2, 0xFFFF, 0xFFFF_FFFF, b.len() as u32 + 1][rng.n(6)];
                let bytes = if rng.n(2) == 0 {
                    v.to_be_bytes()
                } else {
                    v.to_le_bytes()
                };
                let w = [2, 4][rng.n(2)];
                for (i, x) in bytes[4 - w..].iter().enumerate() {
                    if let Some(slot) = b.get_mut(at + i) {
                        *slot = *x;
                    }
                }
            }
            3 => b.truncate(at),
            4 => {
                let len = rng.n(64).min(b.len() - at);
                let piece = b[at..at + len].to_vec();
                let to = rng.n(b.len());
                b.splice(to..to, piece);
            }
            5 => {
                let len = rng.n(64).min(b.len() - at);
                b.drain(at..at + len);
            }
            _ => {
                let to = rng.n(b.len());
                b.insert(to, rng.n(256) as u8);
            }
        }
    }
    b
}

/// Runs `check` on `cases` mutations of each seed, each within `budget`.
fn fuzz(name: &str, seed: u64, seeds: &[Vec<u8>], cases: usize, check: impl Fn(&[u8])) {
    let mut rng = Rng(seed);
    let budget = Duration::from_millis(250);
    for i in 0..cases {
        let input = mutate(&mut rng, &seeds[i % seeds.len()]);
        let start = Instant::now();
        check(&input);
        let took = start.elapsed();
        assert!(took < budget, "{name} case {i} took {took:?}");
    }
}

#[test]
fn jpeg_walker_fuzz() {
    let mut commented = encoded(&gradient(16, 8), Codec::Jpeg);
    commented.splice(2..2, [0xFF, 0xFE, 0, 6, b's', b'e', b'c', b'r']);
    let seeds = vec![
        with_exif(encoded(&gradient(16, 8), Codec::Jpeg), exif(Some(6), true)),
        commented,
    ];
    fuzz("jpeg", 0x0a1b_2c3d_4e5f_6071, &seeds, 4_000, |input| {
        if let Ok(w) = jpeg::walk(input) {
            assert!(w.stripped.len() <= input.len() + 2);
            let again = jpeg::walk(&w.stripped).expect("stripped output walks");
            assert_eq!(again.stripped, w.stripped, "something left to strip");
        }
    });
}

#[test]
fn gif_walker_fuzz() {
    let seeds = vec![
        gif::with_meta(&moving_gif(3, 3, 3), Some(&[1; 300]), Some(b"<xmp/>")),
        {
            let mut g = moving_gif(2, 2, 2);
            g.splice(13..13, *b"\x21\xFE\x03abc\x00");
            g
        },
    ];
    fuzz("gif", 0x1357_9bdf_0246_8ace, &seeds, 4_000, |input| {
        if let Ok(w) = gif::walk(input, true) {
            let again = gif::walk(&w.stripped, true).expect("stripped output walks");
            assert_eq!(again.stripped, w.stripped, "something left to strip");
            assert_eq!(again.xmp, None, "XMP survived");
        }
    });
}

#[test]
fn ico_walker_fuzz() {
    let mut texty =
        img_parts::png::Png::from_bytes(encoded(&rgba(4, 4), Codec::Png).into()).unwrap();
    let at = texty.chunks().len() - 1;
    texty.chunks_mut().insert(
        at,
        img_parts::png::PngChunk::new(*b"tEXt", img_parts::Bytes::from_static(b"k\0v")),
    );
    let seeds = vec![ico_of(&[
        encoded(&rgba(2, 2), Codec::Png),
        texty.encoder().bytes().to_vec(),
    ])];
    fuzz("ico", 0x2468_ace0_1357_9bdf, &seeds, 4_000, |input| {
        if let Ok(icon) = ico::walk(input)
            && let Some(stripped) = ico::strip(input, &icon)
        {
            let again = ico::walk(&stripped).expect("stripped output walks");
            assert!(!again.metadata, "metadata survived");
        }
    });
}

#[test]
fn avif_parser_fuzz() {
    // avif-parse's debug assertions, caught by `avif::parse`, may print as they fire.
    let seeds = vec![avif_with_exif(8, 8, exif(None, true))];
    fuzz("avif", 0x0f1e_2d3c_4b5a_6978, &seeds, 2_000, |input| {
        if let Ok(parsed) = avif::parse(input) {
            let _ = avif::decode_bound(&parsed.primary_item);
        }
        let _ = avif::brand(input);
        if let Ok(m) = avif::meta(input)
            && !m.unresolved
            && let Some(stripped) = avif::strip(input, &m)
        {
            for i in &m.items {
                for &(s, e) in &i.ranges {
                    assert!(stripped[s..e].iter().all(|&b| b == 0), "metadata survived");
                }
            }
        }
    });
}

/// `bytes` probed the way an upload is, from a file.
fn probe_bytes(bytes: &[u8]) -> Result<super::Probe, super::ImageError> {
    let dir = std::env::temp_dir().join(format!("tachyon-fuzz-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("in");
    std::fs::write(&path, bytes).unwrap();
    let out = probe(&path, &bytes[..bytes.len().min(8192)]);
    std::fs::remove_dir_all(&dir).unwrap();
    out
}

/// Runs `check` on each of `cases` within the step budget.
fn each(name: &str, cases: Vec<Vec<u8>>, check: impl Fn(&[u8])) {
    for (i, input) in cases.iter().enumerate() {
        let start = Instant::now();
        check(input);
        let took = start.elapsed();
        assert!(
            took < Duration::from_millis(250),
            "{name} case {i} took {took:?}"
        );
    }
}

/// Every icon entry's declared size set to 8, 33 (a PNG's signature and IHDR), one under
/// and one over, and its offset pointed past the end. One seed is a trap: its PNG entry
/// is followed by a 256 MiB iCCP bomb that only a reader ignoring the declared size
/// reaches, and inflating it takes far longer than the budget.
#[test]
fn icon_sizes_fuzz() {
    let mut trap = ico_around(&iccp_bomb_png(256 << 20));
    trap[14..18].copy_from_slice(&33u32.to_le_bytes());
    let seeds = [
        ico_of(&[
            encoded(&rgba(2, 2), Codec::Png),
            encoded(&rgba(4, 4), Codec::Png),
        ]),
        trap,
    ];
    let mut cases = Vec::new();
    for seed in &seeds {
        let count = usize::from(u16::from_le_bytes([seed[4], seed[5]]));
        for e in 0..count {
            let at = 6 + 16 * e;
            let size = u32::from_le_bytes(seed[at + 8..at + 12].try_into().unwrap());
            for s in [8, 33, size.saturating_sub(1), size + 1] {
                let mut b = seed.clone();
                b[at + 8..at + 12].copy_from_slice(&s.to_le_bytes());
                cases.push(b);
            }
            let mut b = seed.clone();
            b[at + 12..at + 16].copy_from_slice(&(seed.len() as u32 + 1).to_le_bytes());
            cases.push(b);
            if e == 1 {
                // The second entry over the first.
                let mut b = seed.clone();
                b.copy_within(6 + 12..6 + 16, at + 12);
                cases.push(b);
            }
        }
    }
    each("icon sizes", cases, |input| {
        if let Ok(icon) = ico::walk(input) {
            let _ = ico::strip(input, &icon);
        }
        let _ = probe_bytes(input);
    });
}

/// Every `iloc` extent's offset and length set to each of 0, the file's length, one past
/// it and `u32::MAX`.
#[test]
fn avif_extents_fuzz() {
    let seed = avif_with_exif(8, 8, exif(None, true));
    let n = avif_extents(&seed).len();
    let edge = [0, seed.len() as u32, seed.len() as u32 + 1, u32::MAX];
    let mut cases = Vec::new();
    for e in 0..n {
        for &off in &edge {
            for &len in &edge {
                cases.push(avif_set_extent(seed.clone(), e, off, len));
            }
        }
    }
    each("avif extents", cases, |input| {
        if let Ok(m) = avif::meta(input) {
            let _ = avif::strip(input, &m);
        }
        let _ = probe_bytes(input);
    });
}
