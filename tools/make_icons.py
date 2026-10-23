#!/usr/bin/env python3
"""Generate MarKey's menu bar icon without any image library.

The app icon is artwork: app/src-tauri/icons/app-icon.png (1024 px) is the
master, and every bundled size is made from it with

    cd app/src-tauri && cargo tauri icon icons/app-icon.png

The menu bar icon is drawn here instead, because macOS needs a *template*
image: black pixels plus an alpha channel, so it can tint the icon for light
and dark menu bars itself. It repeats the app icon in one colour: a solid key
with A knocked out of it, and in front of it an outlined key with Я.

Everything is rasterised from signed-distance functions with 8x8
supersampling, so the output is deterministic and the repository needs no
binary art tooling.
"""

import math
import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "app/src-tauri/icons"
SS = 8  # supersampling factor per axis

# Geometry in points on the 22 pt template canvas. Angles are clockwise
# degrees, as on screen: the A key leans left, the Я key leans right.
KEY_A = dict(cx=7.5, cy=7.7, size=9.2, radius=2.3, angle=-12)
KEY_YA = dict(cx=14.1, cy=14.1, size=9.2, radius=2.3, angle=10)
GAP = 1.0       # clear space around the front key
RIM = 1.2       # outline width of the front key
STROKE = 0.055  # letter half-width, as a fraction of the key size
CAP = 0.46      # letter height, as a fraction of the key size
NUDGE_A = (-0.07, -0.07)  # A sits towards the uncovered corner of its key


def rounded_rect(w, h, r):
    """Signed distance to a rounded rectangle centred on the origin."""
    def f(px, py):
        dx = abs(px) - (w / 2 - r)
        dy = abs(py) - (h / 2 - r)
        ax, ay = max(dx, 0.0), max(dy, 0.0)
        return math.hypot(ax, ay) + min(max(dx, dy), 0.0) - r
    return f


def capsule(x0, y0, x1, y1, r):
    """Signed distance to a thick line segment with round caps."""
    def f(px, py):
        vx, vy = x1 - x0, y1 - y0
        wx, wy = px - x0, py - y0
        denom = vx * vx + vy * vy
        t = 0.0 if denom == 0 else max(0.0, min(1.0, (wx * vx + wy * vy) / denom))
        return math.hypot(wx - t * vx, wy - t * vy) - r
    return f


def left_arc(cx, cy, radius, r):
    """Signed distance to the left half of a ring: the bowl of Я."""
    def f(px, py):
        if px > cx:
            return math.inf
        return abs(math.hypot(px - cx, py - cy) - radius) - r
    return f


def union(*fns):
    return lambda px, py: min(f(px, py) for f in fns)


def letter_a(r):
    """A drawn with strokes, in a unit key centred on the origin."""
    top, foot, half = -CAP / 2, CAP / 2, CAP * 0.40
    bar = CAP * 0.18
    t = (bar - top) / (foot - top)  # where the crossbar meets the legs
    return union(
        capsule(0, top, -half, foot, r),
        capsule(0, top, half, foot, r),
        capsule(-half * t, bar, half * t, bar, r),
    )


def letter_ya(r):
    """Я drawn with strokes, in a unit key centred on the origin."""
    top, foot = -CAP / 2, CAP / 2
    stem = CAP * 0.30
    mid = CAP * 0.04
    bowl = (mid - top) / 2
    turn = stem - CAP * 0.30   # where the bars bend into the bowl
    return union(
        capsule(stem, top, stem, foot, r),
        capsule(turn, top, stem, top, r),
        capsule(turn, mid, stem, mid, r),
        left_arc(turn, top + bowl, bowl, r),
        capsule(turn + CAP * 0.04, mid, -CAP * 0.36, foot, r),
    )


def shifted(fn, dx, dy):
    return lambda px, py: fn(px - dx, py - dy)


def placed(key, fn, scale):
    """`fn`, drawn in a unit key, moved to `key` on a canvas of `scale` px per pt."""
    a = math.radians(key["angle"])
    cos, sin = math.cos(a), math.sin(a)
    size = key["size"]

    def f(px, py):
        x, y = px / scale - key["cx"], py / scale - key["cy"]
        # Undo the key's rotation, then its size.
        ux, uy = x * cos + y * sin, -x * sin + y * cos
        return fn(ux / size, uy / size) * size
    return f


def key_shape(key, grow=0.0):
    s = key["size"] + 2 * grow
    return rounded_rect(s / key["size"], s / key["size"], (key["radius"] + grow) / key["size"])


def render_template(size):
    """Rasterise the menu bar icon at `size` px, returning RGBA rows."""
    scale = size / 22
    body_a = placed(KEY_A, key_shape(KEY_A), scale)
    mark_a = placed(KEY_A, shifted(letter_a(STROKE), *NUDGE_A), scale)
    body_ya = placed(KEY_YA, key_shape(KEY_YA), scale)
    clear_ya = placed(KEY_YA, key_shape(KEY_YA, GAP), scale)
    mark_ya = placed(KEY_YA, letter_ya(STROKE), scale)

    def inside(x, y):
        d_ya = body_ya(x, y)
        if -RIM <= d_ya <= 0 or mark_ya(x, y) <= 0:
            return True
        return body_a(x, y) <= 0 and mark_a(x, y) > 0 and clear_ya(x, y) > 0

    rows = []
    for py in range(size):
        row = bytearray()
        for px in range(size):
            hits = sum(
                inside(px + (sx + 0.5) / SS, py + (sy + 0.5) / SS)
                for sy in range(SS)
                for sx in range(SS)
            )
            row += bytes((0, 0, 0, round(hits / (SS * SS) * 255)))
        rows.append(bytes(row))
    return rows


def png_bytes(size, rows):
    """The PNG file for `rows`, as bytes."""
    raw = b"".join(b"\x00" + row for row in rows)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for size, name in [(22, "trayTemplate.png"), (44, "trayTemplate@2x.png")]:
        png = png_bytes(size, render_template(size))
        (OUT / name).write_bytes(png)
        print(f"  {name:<20} {size:>3}px  {len(png):>6} bytes  (template)")


if __name__ == "__main__":
    main()
