"""Generate the default playlist cover: the mascot fishing a music note out of a lake at dusk.

The cat comes from the grid in generate.py. Uses only the standard library.

Run from anywhere: python assets/icon/playlist.py
"""

import math
from pathlib import Path

from generate import ACCENT, GRID, HEADPHONES, HIGHLIGHT, VARIANTS, png

OUT = Path(__file__).parent
SIZE = 32
CAT_X, CAT_Y = 15, 10  # The paws land on the dock at y = 23.

BODY, SHADE, EYE = VARIANTS["orange"]
PALETTE = {
    # Cat. These keys match the characters in GRID.
    "#": BODY,
    "D": SHADE,
    "E": EYE,
    "H": HEADPHONES,
    "S": HIGHLIGHT,
    "W": ACCENT,
    "P": (0xF2, 0x6D, 0x8F),
    # Sky, from the top down. Mid-tone purple, so the dark headphones and the orange body both read.
    "a": (0x4F, 0x45, 0x9E),
    "b": (0x6C, 0x57, 0xB8),
    "c": (0x96, 0x6C, 0xC8),
    "d": (0xD0, 0x8A, 0xC4),
    "m": (0xFF, 0xF1, 0xC9),
    "n": (0xEB, 0xD3, 0xA0),
    "s": (0xFF, 0xF8, 0xE3),
    "h": (0x3F, 0x37, 0x80),
    # Lake.
    "w": (0x2F, 0x3F, 0x8F),
    "x": (0x3A, 0x52, 0xA8),
    "r": (0x8F, 0xB4, 0xEE),
    # Dock and fishing rod.
    "o": (0x9A, 0x63, 0x3C),
    "O": (0x6B, 0x42, 0x2A),
    "R": (0x3A, 0x24, 0x18),
    "l": (0xF4, 0xEF, 0xE6),
    "N": (0xFF, 0xF1, 0xC9),
}

NOTE = ["..###", "..#.#", "..#..", "###..", "###.."]
MOON = [".mmm.", "mmmnm", "mnmmm", "mmmnm", ".mmm."]
STARS = [(2, 2), (11, 1), (17, 4), (14, 8), (29, 9), (14, 12), (1, 12), (19, 10)]


def scene():
    grid = [["."] * SIZE for _ in range(SIZE)]

    def put(x, y, ch):
        if 0 <= x < SIZE and 0 <= y < SIZE:
            grid[y][x] = ch

    def sprite(art, x, y, ch):
        for row, line in enumerate(art):
            for col, cell in enumerate(line):
                if cell != ".":
                    put(x + col, y + row, ch if cell == "#" else cell)

    # Sky bands, with sparse pixels on each side of an edge so the bands blend.
    bands = ((0, "a"), (7, "b"), (13, "c"), (18, "d"))
    for y in range(22):
        ch = [band for start, band in bands if y >= start][-1]
        for x in range(SIZE):
            put(x, y, ch)
    for (_, upper), (start, lower) in zip(bands, bands[1:]):
        for x in range(0, SIZE, 4):
            put(x, start - 1, lower)
            put(x + 2, start, upper)

    for x, y in STARS:
        put(x, y, "s")
    sprite(MOON, 22, 2, "m")

    # Far hills along the horizon.
    for x in range(SIZE):
        top = 21 - round(0.8 + 1.2 * math.sin(x * 0.45) + math.sin(x * 0.17 + 1))
        for y in range(top, 22):
            put(x, y, "h")

    # Lake, with a lighter band of ripples.
    for y in range(22, SIZE):
        for x in range(SIZE):
            put(x, y, "x" if y < 25 else "w")
    for x, y, length in ((10, 23, 3), (0, 27, 3), (8, 29, 4), (20, 30, 3), (12, 26, 2)):
        for i in range(length):
            put(x + i, y, "r")

    # Dock.
    for post in (18, 27):
        for y in range(26, 31):
            put(post, y, "O")
            put(post + 1, y, "O")
    for x in range(14, SIZE):
        put(x, 24, "o")
        put(x, 25, "O")

    # Rod: a curve from the cat's paw, bending at the tip.
    start, bend, tip = (15, 21), (7, 6), (4, 8)
    for i in range(61):
        t = i / 60
        x = (1 - t) ** 2 * start[0] + 2 * t * (1 - t) * bend[0] + t**2 * tip[0]
        y = (1 - t) ** 2 * start[1] + 2 * t * (1 - t) * bend[1] + t**2 * tip[1]
        put(round(x), round(y), "R")

    # Fishing line and the catch, fresh out of the water.
    for y in range(9, 15):
        put(4, y, "l")
    sprite(NOTE, 2, 15, "N")
    for x, y in ((1, 21), (8, 19), (7, 22), (0, 18)):
        put(x, y, "r")
    for x in (1, 2, 3, 4, 5, 6, 7):
        put(x, 24, "r")
    put(0, 23, "r")
    put(8, 23, "r")

    # The cat, with a happy face and blush in place of the icon's open eyes. The eyes sit one
    # column further in than the icon's, so they do not merge with the dark headphones.
    for gy, row in enumerate(GRID):
        for gx, ch in enumerate(row):
            if ch != ".":
                put(CAT_X + gx, CAT_Y + gy, "#" if ch == "E" else ch)
    for gx, gy in ((4, 7), (5, 6), (6, 7), (8, 7), (9, 6), (10, 7), (6, 9), (7, 10), (8, 9)):
        put(CAT_X + gx, CAT_Y + gy, "E")
    for gx in (3, 4, 10, 11):
        put(CAT_X + gx, CAT_Y + 8, "P")
    # Paw gripping the rod.
    put(16, 20, "#")
    put(16, 21, "#")
    put(15, 20, "#")
    return grid


def main():
    grid = scene()
    (OUT / "playlist-default.png").write_bytes(png(PALETTE, SIZE, grid))
    (OUT / "playlist-default-512.png").write_bytes(png(PALETTE, 512, grid))


if __name__ == "__main__":
    main()
