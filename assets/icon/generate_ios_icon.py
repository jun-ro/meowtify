#!/usr/bin/env python3
"""Generate the Meowtify iOS app icon.

The Meowtify cat in profile, headphones on, working at a laptop by a window:
warm lamp, pastel city, mug and notebook. Drawn on a 64x64 grid and scaled with
nearest neighbour, so the pixels stay crisp. The output is fully opaque and has
no alpha channel: that is what App Store icons want.

Run from anywhere: python assets/icon/generate_ios_icon.py
"""

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).parent
N = 64
SIZES = (1024, 512, 256, 128)

# ---------------------------------------------------------------- palette ---
SKY = [
    (0, 8, (0x37, 0x8E, 0xA6)),
    (8, 16, (0x45, 0x9C, 0xAE)),
    (16, 24, (0x57, 0xAC, 0xB4)),
    (24, 32, (0x70, 0xBC, 0xB9)),
    (32, 40, (0x8D, 0xC8, 0xBB)),
    (40, 47, (0xAD, 0xD4, 0xBA)),
]

HAZE = (0xA6, 0xC4, 0xC1)
HAZE_HI = (0xBE, 0xD3, 0xC6)
ROOF = (0xA5, 0x6E, 0x58)
ROOF_DARK = (0x87, 0x57, 0x45)
WINDOW_DARK = (0x6E, 0x59, 0x4D)
WINDOW_LIT = (0xFF, 0xD1, 0x7A)

DESK_HI = (0xB8, 0x7F, 0x55)
DESK = (0x99, 0x63, 0x42)
DESK_FRONT = (0x74, 0x49, 0x30)
DESK_DARK = (0x5C, 0x38, 0x24)
SHADOW = (0x54, 0x33, 0x21)

LAMP = (0xC4, 0x48, 0x2F)
LAMP_HI = (0xE8, 0x74, 0x5A)
LAMP_DARK = (0x8B, 0x2C, 0x20)
GLOW = (0xFF, 0xE4, 0xA8)

LEAF = (0x2C, 0x59, 0x4B)
LEAF_HI = (0x3F, 0x78, 0x5D)
POT = (0xC2, 0x6E, 0x50)
POT_DARK = (0x93, 0x49, 0x34)

CREAM = (0xF6, 0xF0, 0xE4)
CREAM_SHADE = (0xD3, 0xC8, 0xB4)
MUZZLE = (0xFF, 0xE0, 0xBC)
INK = (0x3A, 0x3A, 0x44)

LAPTOP = (0x2C, 0x2A, 0x34)
LAPTOP_HI = (0x45, 0x42, 0x52)
SCREEN = (0xC8, 0xEE, 0xF0)
SCREEN_DIM = (0x86, 0xC4, 0xCC)

FUR = (0xF2, 0x9E, 0x4C)
FUR_SHADE = (0xD9, 0x82, 0x3A)
FUR_DARK = (0xB8, 0x6A, 0x2C)
FUR_LIGHT = (0xFF, 0xC2, 0x7C)
EYE = (0x2B, 0x1D, 0x14)
HP = (0x17, 0x17, 0x1C)
HP_HI = (0x3D, 0x3D, 0x48)
ACCENT = (0xFF, 0xFF, 0xFF)

# ---------------------------------------------------------------- canvas ----
px = [[(0, 0, 0)] * N for _ in range(N)]


def put(x, y, c):
    if 0 <= x < N and 0 <= y < N:
        px[y][x] = c


def rect(x, y, w, h, c):
    for yy in range(y, y + h):
        for xx in range(x, x + w):
            put(xx, yy, c)


def hline(y, x0, x1, c):
    for x in range(x0, x1 + 1):
        put(x, y, c)


def vline(x, y0, y1, c):
    for y in range(y0, y1 + 1):
        put(x, y, c)


def shade(c, amount):
    return tuple(max(0, min(255, v + amount)) for v in c)


def ellipse(cx, cy, rx, ry, c, ymax=N - 1, ymin=0):
    for y in range(max(ymin, int(cy - ry)), min(ymax, int(cy + ry)) + 1):
        for x in range(int(cx - rx), int(cx + rx) + 1):
            if ((x - cx) / rx) ** 2 + ((y - cy) / ry) ** 2 <= 1.0:
                put(x, y, c)


def rrect(x, y, w, h, r, c):
    for yy in range(y, y + h):
        for xx in range(x, x + w):
            dx = min(xx - x, x + w - 1 - xx)
            dy = min(yy - y, y + h - 1 - yy)
            if dx + dy >= r:
                put(xx, yy, c)


def outline(colors, color):
    """Draw a 1px outline around every region made of `colors`."""
    mask = {(x, y) for y in range(N) for x in range(N) if px[y][x] in colors}
    for x, y in mask:
        for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            nx, ny = x + dx, y + dy
            if 0 <= nx < N and 0 <= ny < N and (nx, ny) not in mask:
                put(nx, ny, color)


def tri(a, b, c_, color):
    xs = (a[0], b[0], c_[0])
    ys = (a[1], b[1], c_[1])
    d = (b[1] - c_[1]) * (a[0] - c_[0]) + (c_[0] - b[0]) * (a[1] - c_[1])
    if d == 0:
        return
    for y in range(min(ys), max(ys) + 1):
        for x in range(min(xs), max(xs) + 1):
            l1 = ((b[1] - c_[1]) * (x - c_[0]) + (c_[0] - b[0]) * (y - c_[1])) / d
            l2 = ((c_[1] - a[1]) * (x - c_[0]) + (a[0] - c_[0]) * (y - c_[1])) / d
            if l1 >= -0.15 and l2 >= -0.15 and 1 - l1 - l2 >= -0.15:
                put(x, y, color)


# ------------------------------------------------------------------- sky ----
def sky():
    for y0, y1, c in SKY:
        rect(0, y0, N, y1 - y0, c)


# ------------------------------------------------------------------ city ----
DISTANT = [
    (0, 9, 20), (7, 8, 15), (14, 9, 22), (22, 10, 17),
    (31, 9, 21), (39, 10, 16), (48, 9, 20), (56, 8, 15),
]
FRONT = [
    # x, width, top, body, lit windows
    (0, 11, 24, (0xE9, 0xDC, 0xC8), 2),
    (10, 8, 18, (0xD8, 0xC2, 0xB2), 1),
    (17, 10, 26, (0xF2, 0xE8, 0xD8), 0),
    (26, 11, 21, (0xC2, 0xAC, 0x9C), 0),
    (36, 10, 25, (0xE9, 0xDC, 0xC8), 0),
    (45, 10, 19, (0xF2, 0xE8, 0xD8), 2),
    (54, 10, 23, (0xD8, 0xC2, 0xB2), 1),
]


def windows(x, y, w, h, seed, lit):
    n = 0
    for yy in range(y + 4, y + h - 3, 5):
        for xx in range(x + 2, x + w - 2, 4):
            n += 1
            if (xx * 5 + yy * 3 + seed) % 4 == 0:
                continue
            put(xx, yy, WINDOW_LIT if (n + seed) % 8 < lit else WINDOW_DARK)
            if (xx + yy + seed) % 3:
                put(xx, yy + 1, shade(WINDOW_DARK, -10))


def city():
    for x, w, top in DISTANT:
        rect(x, top, w, 47 - top, HAZE)
        hline(top, x, x + w - 1, HAZE_HI)
        if w > 6:
            rect(x + w - 3, top - 4, 2, 4, HAZE)

    for i, (x, w, top, body, lit) in enumerate(FRONT):
        rect(x, top, w, 47 - top, body)
        rect(x, top - 2, w, 2, ROOF)
        hline(top - 3, x, x + w - 1, ROOF_DARK)
        hline(top, x, x + w - 1, shade(body, 14))
        windows(x, top, w, 47 - top, i * 3, lit)
        vline(x, top, 46, shade(body, -14))


# ------------------------------------------------------------------ desk ----
def desk():
    rect(0, 46, N, 18, DESK)
    hline(46, 0, N - 1, DESK_HI)
    hline(47, 0, N - 1, shade(DESK_HI, -10))
    rect(0, 48, N, 16, DESK_FRONT)
    for y, x0, x1 in ((52, 4, 18), (57, 30, 48), (60, 8, 26),
                      (63, 38, 60), (54, 50, 62)):
        hline(y, x0, x1, DESK_DARK)


# ------------------------------------------------------------------ lamp ----
def lamp():
    vline(5, 0, 1, SHADOW)
    put(5, 2, LAMP)
    rows = {2: (4, 11), 3: (3, 12), 4: (2, 13), 5: (1, 14),
            6: (0, 15), 7: (0, 15), 8: (0, 15)}
    for y, (x0, x1) in rows.items():
        hline(y, x0, x1, LAMP)
    hline(2, 5, 10, LAMP_HI)
    hline(3, 4, 6, LAMP_HI)
    hline(4, 3, 5, LAMP_HI)
    hline(7, 0, 15, LAMP_DARK)
    hline(8, 1, 14, LAMP_DARK)
    hline(8, 5, 10, GLOW)
    hline(9, 6, 9, (0xFF, 0xF6, 0xDC))
    hline(10, 6, 9, GLOW)


# ----------------------------------------------------------------- plant ----
def plant():
    shape = {
        20: (56, 59),
        21: (55, 60),
        22: (54, 61),
        23: (53, 62),
        24: (54, 61),
        25: (53, 60),
        26: (54, 59),
        27: (55, 58),
    }
    for y, (x0, x1) in shape.items():
        hline(y, x0, x1, LEAF)
    for x, y in ((55, 21), (56, 22), (57, 22), (54, 23), (58, 24),
                 (55, 25), (59, 24), (57, 20), (58, 20)):
        put(x, y, LEAF_HI)
    for x, y in ((53, 23), (62, 23), (53, 25), (60, 25), (54, 26), (59, 26)):
        put(x, y, shade(LEAF, -10))
    vline(57, 27, 29, LEAF)
    rect(53, 29, 9, 7, POT)
    hline(29, 52, 62, POT)
    hline(30, 52, 62, shade(POT, 12))
    hline(35, 53, 61, POT_DARK)
    vline(61, 30, 34, POT_DARK)


# ------------------------------------------------------------------- cat ----
# The Meowtify logo cat, seen from the side: blocky square head, one ear, the
# headphone band across the top, the ear cup with its white accent, one eye,
# a blocky nose, and the little fringe of legs along the bottom.
def cat():
    # tail sticking out behind
    rrect(0, 32, 11, 4, 1, FUR)
    hline(35, 1, 10, FUR_SHADE)

    # the cube: one block, like the logo
    rect(10, 18, 27, 25, FUR)

    # the logo's fringe of legs
    for x0 in (11, 18, 25, 32):
        rect(x0, 42, 5, 3, FUR)

    # ear
    rect(13, 15, 8, 3, FUR)
    rect(14, 13, 6, 2, FUR)
    rect(15, 11, 4, 2, FUR)

    # headphone band
    rect(10, 18, 27, 5, HP)
    hline(18, 12, 35, HP_HI)

    # ear cup on the near side
    rrect(11, 23, 8, 11, 2, HP)
    hline(24, 12, 17, HP_HI)
    vline(12, 25, 32, HP_HI)
    rect(13, 26, 2, 3, ACCENT)

    # eye, nose, mouth
    rect(30, 27, 2, 4, EYE)
    rect(36, 31, 2, 2, EYE)
    rect(33, 34, 2, 1, EYE)

    # lamp light on the back, shade underneath
    vline(10, 24, 41, FUR_LIGHT)
    vline(2, 33, 34, FUR_LIGHT)
    hline(41, 11, 35, FUR_SHADE)
    hline(44, 11, 35, FUR_SHADE)
    hline(45, 13, 33, SHADOW)


# ---------------------------------------------------------------- laptop ----
def laptop():
    # keyboard deck
    rect(36, 43, 26, 4, LAPTOP)
    hline(43, 36, 61, LAPTOP_HI)
    for x in range(39, 58, 3):
        put(x, 44, LAPTOP_HI)
        put(x, 45, LAPTOP_HI)

    # lid, leaning back a little
    for y in range(21, 43):
        off = round((42 - y) * 2 / 21)
        hline(y, 48 + off, 60 + off, LAPTOP)
        put(48 + off, y, LAPTOP_HI)
    hline(21, 50, 62, LAPTOP_HI)

    # little meowtify cat on the lid
    sticker = [
        ".F..F.",
        "FFFFFF",
        "FFFFFF",
        "F.FF.F",
        "FFFFFF",
        ".FFFF.",
    ]
    for dy, row in enumerate(sticker):
        for dx, ch in enumerate(row):
            if ch == "F":
                put(51 + dx, 29 + dy, FUR)
    put(52, 30, EYE)
    put(55, 30, EYE)


# ------------------------------------------------------------------ mug -----
def mug():
    x0, y0 = 47, 49
    rect(x0, y0, 11, 11, CREAM)
    vline(x0 + 10, y0 + 1, y0 + 9, CREAM_SHADE)
    hline(y0 + 10, x0, x0 + 10, CREAM_SHADE)
    hline(y0, x0 + 1, x0 + 9, (0xFF, 0xFC, 0xF4))
    for y in range(y0 + 2, y0 + 8):
        put(x0 + 11, y, CREAM_SHADE if y in (y0 + 2, y0 + 7) else CREAM)
        put(x0 + 12, y, CREAM_SHADE if y in (y0 + 2, y0 + 7) else CREAM)
    rect(x0 + 1, y0 + 3, 9, 5, INK)
    for x, y in ((x0 + 5, y0 + 3), (x0 + 5, y0 + 4), (x0 + 5, y0 + 5),
                 (x0 + 4, y0 + 6), (x0 + 3, y0 + 6),
                 (x0 + 6, y0 + 3), (x0 + 7, y0 + 4)):
        put(x, y, CREAM)
    put(x0 + 2, y0 - 2, CREAM_SHADE)
    put(x0 + 3, y0 - 4, CREAM_SHADE)
    put(x0 + 8, y0 - 2, CREAM_SHADE)


# -------------------------------------------------------------- notebook ----
def notebook():
    x0, y0 = 3, 50
    rect(x0, y0, 16, 11, INK)
    rect(x0 + 1, y0 + 1, 14, 8, CREAM)
    vline(x0 + 8, y0 + 1, y0 + 8, CREAM_SHADE)
    for y in (y0 + 2, y0 + 4, y0 + 6):
        hline(y, x0 + 2, x0 + 6, CREAM_SHADE)
        hline(y, x0 + 10, x0 + 13, CREAM_SHADE)
    for i in range(9):
        put(x0 + 3 + i, y0 + 7 - i // 5, (0xE8, 0xA0, 0x3C))
    put(x0 + 12, y0 + 5, (0xE8, 0xA0, 0x3C))
    put(x0 + 12, y0 + 6, INK)
    hline(y0 + 10, x0 + 1, x0 + 14, shade(INK, 12))


# ------------------------------------------------------------------- out ----
def build():
    sky()
    city()
    desk()
    lamp()
    laptop()
    cat()
    return px


def png(scale, out_path):
    """Nearest-neighbour scale to scale*N and encode an opaque RGB PNG."""
    size = scale * N
    rows = []
    for row in px:
        line = b"\x00"
        for r, g, b in row:
            line += bytes((r, g, b)) * scale
        rows.extend([line] * scale)

    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data)))

    data = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
        + chunk(b"IEND", b"")
    )
    out_path.write_bytes(data)


def main():
    build()
    for size in SIZES:
        assert size % N == 0, size
        png(size // N, OUT / f"meowtify-ios-{size}.png")
        print(f"wrote meowtify-ios-{size}.png")


if __name__ == "__main__":
    main()
