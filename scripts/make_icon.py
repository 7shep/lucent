#!/usr/bin/env python3
"""Write src-tauri/icons/icon.ico: a dark glossy pill glyph.

The ICO holds two PNG entries (256x256 and 32x32). Standard library only.
"""

import math
import os
import struct
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_PATH = os.path.join(ROOT, "src-tauri", "icons", "icon.ico")
SIZES = (256, 32)


def sd_rounded_rect(px, py, cx, cy, half_w, half_h, radius):
    """Signed distance from a point to a rounded rectangle (negative inside)."""
    qx = abs(px - cx) - (half_w - radius)
    qy = abs(py - cy) - (half_h - radius)
    outside = math.hypot(max(qx, 0.0), max(qy, 0.0))
    inside = min(max(qx, qy), 0.0)
    return outside + inside - radius


def coverage(distance):
    """Anti-aliased coverage in [0, 1] from a signed distance in pixels."""
    return min(max(0.5 - distance, 0.0), 1.0)


def mix(a, b, t):
    return a + (b - a) * t


def over(dst, src):
    """Composite straight-alpha RGBA src over dst (floats in 0..1)."""
    sa = src[3]
    da = dst[3]
    out_a = sa + da * (1.0 - sa)
    if out_a <= 0.0:
        return [0.0, 0.0, 0.0, 0.0]
    return [
        (src[0] * sa + dst[0] * da * (1.0 - sa)) / out_a,
        (src[1] * sa + dst[1] * da * (1.0 - sa)) / out_a,
        (src[2] * sa + dst[2] * da * (1.0 - sa)) / out_a,
        out_a,
    ]


def to_byte(value):
    return int(round(min(max(value, 0.0), 1.0) * 255))


def render(size):
    """Return the scanlines (RGBA bytes, one bytes object per row)."""
    s = float(size)
    half = s / 2.0
    pill_hw = 0.36 * s
    pill_hh = 0.14 * s
    dot_x = half - pill_hw * 0.5
    dot_r = 0.085 * s
    rows = []
    for y in range(size):
        py = y + 0.5
        row = bytearray()
        for x in range(size):
            px = x + 0.5
            color = [0.0, 0.0, 0.0, 0.0]

            # Rounded-square tile: graphite fading toward black.
            a = coverage(sd_rounded_rect(px, py, half, half, half, half, 0.22 * s))
            if a > 0.0:
                g = mix(0.17, 0.04, py / s)
                color = over(color, [g, g, g, a])

            # Glossy pill: near-black body, lighter upper sheen, thin rim light.
            d = sd_rounded_rect(px, py, half, half, pill_hw, pill_hh, pill_hh)
            a = coverage(d)
            if a > 0.0:
                t = min(max((py - (half - pill_hh)) / (2.0 * pill_hh), 0.0), 1.0)
                g = mix(0.14, 0.015, t)
                sheen = max(0.0, 0.5 - t) * 0.5
                rim = 0.0
                if d < 0.0:
                    rim = max(0.0, 1.0 - (-d) / (0.012 * s))
                lift = min(sheen + 0.35 * rim, 1.0)
                c = g + (1.0 - g) * lift
                color = over(color, [c, c, c, a])

            # Small lens glint on the left of the pill.
            a = coverage(math.hypot(px - dot_x, py - half) - dot_r)
            if a > 0.0:
                color = over(color, [0.95, 0.96, 1.0, a])

            row.extend(
                (to_byte(color[0]), to_byte(color[1]), to_byte(color[2]), to_byte(color[3]))
            )
        rows.append(bytes(row))
    return rows


def png_chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)


def png_bytes(size, rows):
    raw = b"".join(b"\x00" + row for row in rows)  # filter type 0 per scanline
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", ihdr)
        + png_chunk(b"IDAT", zlib.compress(raw, 9))
        + png_chunk(b"IEND", b"")
    )


def ico_bytes(entries):
    """entries: list of (size, png_data). Vista+ ICO files may embed PNG data."""
    header = struct.pack("<HHH", 0, 1, len(entries))
    directory = b""
    blobs = b""
    offset = 6 + 16 * len(entries)
    for size, png in entries:
        dim = 0 if size >= 256 else size  # 0 means 256 in ICO directory entries
        directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset)
        offset += len(png)
        blobs += png
    return header + directory + blobs


def main():
    entries = [(size, png_bytes(size, render(size))) for size in SIZES]
    os.makedirs(os.path.dirname(OUT_PATH), exist_ok=True)
    with open(OUT_PATH, "wb") as handle:
        handle.write(ico_bytes(entries))
    print(f"wrote {OUT_PATH} ({os.path.getsize(OUT_PATH)} bytes)")


if __name__ == "__main__":
    main()
