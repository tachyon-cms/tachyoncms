//! An icon's directory and the chunks of its PNG entries, walked within fixed bounds, and
//! the icon rebuilt with those PNGs' metadata chunks gone and every entry kept.

use super::{ImageError, PNG_KEEP};

/// The most entries an icon may list: real ones hold a handful of sizes.
const MAX_ENTRIES: usize = 256;
/// The most PNG chunks walked over all of an icon's entries.
const MAX_CHUNKS: usize = 1 << 16;

/// A PNG chunk: its type and where it starts and ends (length to CRC, inclusive).
pub(super) type Chunk = ([u8; 4], usize, usize);

/// The whole chunks of PNG `data`, up to a cut-off one if there is one, counting each
/// against `budget` and refusing past it.
pub(super) fn png_chunks(data: &[u8], budget: &mut usize) -> Result<Vec<Chunk>, ImageError> {
    let mut out = Vec::new();
    let mut i = 8;
    while i < data.len() {
        *budget = budget
            .checked_sub(1)
            .ok_or_else(|| ImageError::Corrupt("this image has too many PNG chunks".into()))?;
        let Some(h) = data.get(i..i + 8) else { break };
        let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let Some(end) = i.checked_add(12).and_then(|e| e.checked_add(len)) else {
            break;
        };
        if end > data.len() {
            break;
        }
        let kind = [h[4], h[5], h[6], h[7]];
        out.push((kind, i, end));
        if &kind == b"IEND" {
            break;
        }
        i = end;
    }
    Ok(out)
}

/// One directory entry: its 16 bytes, and the range of its image.
pub(super) struct Entry {
    dir: [u8; 16],
    start: usize,
    end: usize,
    /// For a PNG entry, its chunks.
    chunks: Option<Vec<Chunk>>,
}

/// An icon, walked.
pub(super) struct Icon {
    entries: Vec<Entry>,
    /// Whether any PNG entry carries a chunk outside `PNG_KEEP`.
    pub metadata: bool,
}

/// Walks the icon `bytes`. Each distinct image is walked once however many entries point
/// at it. Refuses more than `MAX_ENTRIES` entries, more than `MAX_CHUNKS` chunks in all,
/// and any PNG entry with compressed metadata (`iCCP`, `zTXt`, `iTXt`), which the icon
/// decoder would inflate without a limit.
pub(super) fn walk(bytes: &[u8]) -> Result<Icon, ImageError> {
    let cut = || ImageError::Corrupt("the icon's directory is cut off".into());
    let count = usize::from(u16::from_le_bytes(
        bytes
            .get(4..6)
            .ok_or_else(cut)?
            .try_into()
            .map_err(|_| cut())?,
    ));
    if count > MAX_ENTRIES {
        return Err(ImageError::Corrupt(format!(
            "this icon lists {count} images; at most {MAX_ENTRIES} are taken"
        )));
    }
    let mut budget = MAX_CHUNKS;
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    let mut metadata = false;
    for e in 0..count {
        let dir: [u8; 16] = bytes
            .get(6 + 16 * e..22 + 16 * e)
            .ok_or_else(cut)?
            .try_into()
            .map_err(|_| cut())?;
        let size = u32::from_le_bytes([dir[8], dir[9], dir[10], dir[11]]) as usize;
        let start = u32::from_le_bytes([dir[12], dir[13], dir[14], dir[15]]) as usize;
        let end = start.checked_add(size).ok_or_else(cut)?;
        let data = bytes.get(start..end).ok_or_else(cut)?;
        if let Some(seen) = entries.iter().find(|x| (x.start, x.end) == (start, end)) {
            let chunks = seen.chunks.clone();
            entries.push(Entry {
                dir,
                start,
                end,
                chunks,
            });
            continue;
        }
        let chunks = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            let chunks = png_chunks(data, &mut budget)?;
            for (kind, _, _) in &chunks {
                if matches!(kind, b"iCCP" | b"zTXt" | b"iTXt") {
                    return Err(ImageError::Corrupt(
                        "an icon's PNG carries compressed metadata".into(),
                    ));
                }
                metadata |= !PNG_KEEP.contains(&kind);
            }
            Some(chunks)
        } else {
            None
        };
        entries.push(Entry {
            dir,
            start,
            end,
            chunks,
        });
    }
    Ok(Icon { entries, metadata })
}

/// The icon `bytes` rebuilt with its PNG entries' chunks outside `PNG_KEEP` gone, every
/// entry kept, or `None` when there's nothing to take out.
pub(super) fn strip(bytes: &[u8], icon: &Icon) -> Option<Vec<u8>> {
    if !icon.metadata {
        return None;
    }
    let header = bytes.get(..6)?;
    let mut images: Vec<((usize, usize), Vec<u8>)> = Vec::new();
    for e in &icon.entries {
        if images.iter().any(|(r, _)| *r == (e.start, e.end)) {
            continue;
        }
        let data = bytes.get(e.start..e.end)?;
        let image = match &e.chunks {
            Some(chunks) => {
                let mut out = data.get(..8)?.to_vec();
                for (kind, s, end) in chunks {
                    if PNG_KEEP.contains(&kind) {
                        out.extend_from_slice(data.get(*s..*end)?);
                    }
                }
                out
            }
            None => data.to_vec(),
        };
        images.push(((e.start, e.end), image));
    }
    let mut out = header.to_vec();
    let mut at = 6 + 16 * icon.entries.len();
    let mut placed: Vec<((usize, usize), usize)> = Vec::new();
    for (range, image) in &images {
        placed.push((*range, at));
        at += image.len();
    }
    for e in &icon.entries {
        let (i, &(_, offset)) = placed
            .iter()
            .enumerate()
            .find(|(_, (r, _))| *r == (e.start, e.end))?;
        let mut dir = e.dir;
        dir[8..12].copy_from_slice(&u32::try_from(images[i].1.len()).ok()?.to_le_bytes());
        dir[12..16].copy_from_slice(&u32::try_from(offset).ok()?.to_le_bytes());
        out.extend_from_slice(&dir);
    }
    for (_, image) in &images {
        out.extend_from_slice(image);
    }
    Some(out)
}
