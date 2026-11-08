"""Generate the server icon: the mascot at a laptop, next to a server rack.

The cat comes from the grid in generate.py. Uses only the standard library.

Run from anywhere: python assets/icon/server.py
"""

from pathlib import Path

from generate import ACCENT, GRID, HEADPHONES, HIGHLIGHT, VARIANTS, png

OUT = Path(__file__).parent
SIZE = 32
FLOOR = 29  # First row below everything that stands on the floor.
CAT_X, CAT_Y = 15, FLOOR - 14

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
    # Rack. The frame is light enough to read on a dark sidebar.
    "f": (0x5A, 0x5A, 0x6C),
    "F": (0x7A, 0x7A, 0x8E),
    "u": (0x2E, 0x2E, 0x38),
    "v": (0x45, 0x45, 0x52),
    "c": (0x5C, 0xE0, 0xD8),  # Status lights.
    "a": (0xFF, 0x8C, 0x1A),
    # Laptop: plain black, with a lighter top edge so it stays apart from the headphones.
    "l": (0x1C, 0x1C, 0x22),
    "L": (0x4A, 0x4A, 0x58),
    "k": (0x2E, 0x2E, 0x38),
    # Cable and floor shadow.
    "n": (0x9B, 0x7B, 0xF0),
    "s": (0x4A, 0x4A, 0x55),
}

# Status lights per rack unit, so the units do not all look the same.
LIGHTS = [("c", "a"), ("c", "c"), ("a", "c"), ("c", "P"), ("c", "c")]


def scene():
    grid = [["."] * SIZE for _ in range(SIZE)]

    def put(x, y, key):
        if 0 <= x < SIZE and 0 <= y < SIZE:
            grid[y][x] = key

    def rect(x0, y0, x1, y1, key):
        for y in range(y0, y1 + 1):
            for x in range(x0, x1 + 1):
                put(x, y, key)

    def sprite(art, x, y, key):
        for row, line in enumerate(art):
            for col, cell in enumerate(line):
                if cell == "#":
                    put(x + col, y + row, key)

    # Floor shadow under the rack and the cat.
    rect(1, FLOOR, 30, FLOOR, "s")

    # Server rack: a frame with five units, each with two lights and a vent.
    rect(1, 7, 11, FLOOR - 1, "f")
    rect(1, 7, 11, 7, "F")
    for i, (left, right) in enumerate(LIGHTS):
        top = 9 + i * 4
        rect(2, top, 10, top + 2, "u")
        put(3, top + 1, left)
        put(5, top + 1, right)
        for x in range(7, 10):
            put(x, top + 1, "v")
    put(2, FLOOR - 1, "u")
    put(10, FLOOR - 1, "u")

    # The cat, looking down at the screen.
    for gy, row in enumerate(GRID):
        for gx, ch in enumerate(row):
            if ch != ".":
                put(CAT_X + gx, CAT_Y + gy, "#" if ch == "E" else ch)
    for gx in (4, 10):
        put(CAT_X + gx, CAT_Y + 7, "E")
    for gx in (3, 4, 10, 11):
        put(CAT_X + gx, CAT_Y + 8, "P")

    # Laptop, seen from behind the lid.
    rect(16, 24, 28, 27, "l")
    rect(16, 24, 28, 24, "L")
    rect(15, 28, 29, 28, "k")

    # Cable from the rack to the laptop, along the floor.
    for x in range(12, 15):
        put(x, 27 if x == 12 else 28, "n")
    put(12, 26, "n")
    return grid


def main():
    grid = scene()
    (OUT / "server.png").write_bytes(png(PALETTE, SIZE, grid))
    (OUT / "server-512.png").write_bytes(png(PALETTE, 512, grid))


if __name__ == "__main__":
    main()
