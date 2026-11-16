#!/usr/bin/env python3
"""Makes the committed upload fixtures in this directory. See README.md.

Needs ffmpeg, gif2webp (libwebp) and, for the CMYK JPEG, macOS's sips. Only the
standard library otherwise: the EXIF blocks are written by hand, with made-up values.
"""

import os
import struct
import subprocess
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))


def run(*args):
    subprocess.run(args, check=True, capture_output=True)


def ffmpeg_jpeg(out, w, h):
    # A test pattern, one frame, baseline JPEG.
    run("ffmpeg", "-y", "-loglevel", "error", "-f", "lavfi",
        "-i", f"testsrc=size={w}x{h}:rate=1", "-frames:v", "1",
        "-pix_fmt", "yuvj420p", out)


def exif(orientation=None, gps=False):
    """A big-endian TIFF block: Orientation, and a GPS IFD with a made-up latitude."""
    ifd0 = []
    if orientation is not None:
        ifd0.append(struct.pack(">HHIHH", 0x0112, 3, 1, orientation, 0))
    count = len(ifd0) + (1 if gps else 0)
    ifd0_end = 8 + 2 + 12 * count + 4
    if gps:
        ifd0.append(struct.pack(">HHII", 0x8825, 4, 1, ifd0_end))
    out = b"MM\x00\x2a" + struct.pack(">I", 8)
    out += struct.pack(">H", count) + b"".join(ifd0) + b"\x00" * 4
    if gps:
        # GPSLatitudeRef "N", GPSLatitude 1/1 2/1 3/1 (1°2'3", nowhere anyone lives).
        rationals_at = len(out) + 2 + 12 * 2 + 4
        out += struct.pack(">H", 2)
        out += struct.pack(">HHI2s2x", 0x0001, 2, 2, b"N\x00")
        out += struct.pack(">HHII", 0x0002, 5, 3, rationals_at)
        out += b"\x00" * 4
        out += struct.pack(">6I", 1, 1, 2, 1, 3, 1)
    return out


def with_exif(jpeg, block):
    """`jpeg` with an APP1 EXIF segment after SOI and the JFIF APP0, where cameras put it."""
    body = b"Exif\x00\x00" + block
    app1 = b"\xff\xe1" + struct.pack(">H", len(body) + 2) + body
    at = 2
    if jpeg[2:4] == b"\xff\xe0":
        at += 2 + struct.unpack(">H", jpeg[4:6])[0]
    return jpeg[:at] + app1 + jpeg[at:]


def without_app2(jpeg):
    """`jpeg` without its APP2 (ICC profile) segments."""
    out, i = bytearray(jpeg[:2]), 2
    while i < len(jpeg) and jpeg[i] == 0xFF and jpeg[i + 1] not in (0xDA,):
        marker = jpeg[i + 1]
        length = struct.unpack(">H", jpeg[i + 2:i + 4])[0]
        if marker != 0xE2:
            out += jpeg[i:i + 2 + length]
        i += 2 + length
    return bytes(out + jpeg[i:])


def write(name, data):
    with open(os.path.join(HERE, name), "wb") as f:
        f.write(data)


def read(path):
    with open(path, "rb") as f:
        return f.read()


with tempfile.TemporaryDirectory() as tmp:
    plain = os.path.join(tmp, "plain.jpg")
    ffmpeg_jpeg(plain, 32, 16)
    write("gps.jpg", with_exif(read(plain), exif(gps=True)))

    wide = os.path.join(tmp, "wide.jpg")
    ffmpeg_jpeg(wide, 64, 32)
    write("turned.jpg", with_exif(read(wide), exif(orientation=6)))

    rgb = os.path.join(tmp, "rgb.jpg")
    ffmpeg_jpeg(rgb, 24, 16)
    cmyk = os.path.join(tmp, "cmyk.jpg")
    run("sips", "-m", "/System/Library/ColorSync/Profiles/Generic CMYK Profile.icc",
        rgb, "--out", cmyk)
    write("cmyk.jpg", without_app2(read(cmyk)))

    gif = os.path.join(HERE, "moving.gif")
    run("ffmpeg", "-y", "-loglevel", "error", "-f", "lavfi",
        "-i", "testsrc=size=16x12:rate=10", "-frames:v", "3", "-loop", "0", gif)
    run("gif2webp", "-quiet", "-lossy", gif, "-o", os.path.join(HERE, "moving.webp"))

    png = os.path.join(tmp, "whole.png")
    run("ffmpeg", "-y", "-loglevel", "error", "-f", "lavfi",
        "-i", "testsrc=size=32x32:rate=1", "-frames:v", "1", png)
    whole = read(png)
    write("truncated.png", whole[: len(whole) // 2])
