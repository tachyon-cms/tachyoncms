# Upload fixtures

Small files for `tests/uploads_leaks.rs` (`fixture_corpus`). Each binary one is made
by an encoder other than ours, so the tests read what real tools write; the SVGs are
written by hand. All are under 2 KB.
The EXIF is written by hand with made-up values: no real place or camera is in them.

`make.py` makes the binary files again. It needs ffmpeg, gif2webp (libwebp) and, for
the CMYK JPEG, macOS's `sips`.

| File | How it's made | What the test expects |
| --- | --- | --- |
| `gps.jpg` | ffmpeg's 32×16 test pattern, plus an APP1 EXIF block with a GPS IFD (latitude N 1°2'3") | Stored without EXIF; kept byte for byte with `withMetadata` |
| `turned.jpg` | ffmpeg's 64×32 test pattern, plus EXIF orientation 6 | Stored upright, 32×64, without EXIF; kept as is with `withMetadata`, reported as 32×64 |
| `cmyk.jpg` | ffmpeg's 24×16 test pattern, converted by `sips` to the Generic CMYK profile, with the profile's APP2 segments removed to keep it small | 4-component JPEG, Adobe marker; decodes, 24×16 |
| `moving.gif` | ffmpeg, 3 frames of a 16×12 test pattern, looping | Stays animated: 3 frames |
| `moving.webp` | `gif2webp -lossy moving.gif` | Stays animated: 3 frames |
| `truncated.png` | ffmpeg's 32×32 PNG, cut to its first half | 422: "this image is cut off before its end" |
| `style-script.svg` | Written by hand: a `<script>` inside `<style>`, and one hidden in a CDATA section | Stored without either script; served with `sandbox` |
| `external-use.svg` | Written by hand: `<use>` pointing at another host, a protocol-relative URL and a `data:` URL, beside one local `#r` | Only the local `<use>` keeps its `href` |

Made on the spot by `tests/common/fixtures.rs`, so they aren't committed:

- **Header bombs**: PNG, JPEG, GIF, WebP and BMP files of a few hundred bytes whose
  headers claim 100 million pixels or more. Each is refused from its header, before
  anything is decoded.
- **Frame bombs**: a GIF and an animated WebP of 51 frames on a 1000×1000 canvas, so their
  frames add up to 51 million pixels, one million over the cap.
- **Polyglots**: a PNG followed by an HTML page, and a PNG followed by a ZIP archive
  holding `index.html`. Both are stored and served as `image/png` with `nosniff`. What
  follows the PNG's `IEND` isn't metadata, so it's dropped, with `withMetadata` or not.

The WebP header bomb claims 10000×10000, not 16384×16384: the image crate reads a 16384
header as 0×0, and the upload is refused as a broken file rather than by the pixel cap.
That fails closed too, but wouldn't test the cap.
