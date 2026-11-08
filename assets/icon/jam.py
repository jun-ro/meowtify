"""Generate the Jam icon: two mascots, orange and purple, listening together.

The cats come from the grid in generate.py. Uses only the standard library.

Run from anywhere: python assets/icon/jam.py
"""

from pathlib import Path

from generate import ACCENT, GRID, HEADPHONES, HIGHLIGHT, VARIANTS, png

OUT = Path(__file__).parent
SIZE = 32

PALETTE = {
    "H": HEADPHONES,
    "S": HIGHLIGHT,
    "W": ACCENT,
    "P": (0xF2, 0x6D, 0x8F),  # Heart and blush.
    "p": (0xFF, 0xB3, 0xC6),  # Heart shine.
    "n": (0x9B, 0x7B, 0xF0),  # Note near the orange cat.
    "N": (0xF2, 0x9E, 0x4C),  # Note near the purple cat.
}
# Each cat gets its own body, shade and eye keys.
for prefix, name in (("o", "orange"), ("u", "purple")):
    body, shade, eye = VARIANTS[name]
    PALETTE[prefix + "#"] = body
    PALETTE[prefix + "D"] = shade
    PALETTE[prefix + "E"] = eye

HEART = [".##..##.", "#p######", "########", ".######.", "..####..", "...##..."]
NOTE = ["..###", "..#.#", "..#..", "###..", "###.."]
NOTES = [".####", ".#..#", ".#..#", "##.##", "##.##"]
SPARKLE = [".#.", "###", ".#."]
# The icon's open eyes plus blush, in grid coordinates. Eyes in column 3 would merge with the
# headphones, so the cats look straight ahead.
FACE = {
    "E": [(4, 6), (4, 7), (10, 6), (10, 7)],
    "P": [(3, 8), (4, 8), (10, 8), (11, 8)],
}


def scene():
    grid = [["."] * SIZE for _ in range(SIZE)]

    def put(x, y, key):
        if 0 <= x < SIZE and 0 <= y < SIZE:
            grid[y][x] = key

    def sprite(art, x, y, key):
        for row, line in enumerate(art):
            for col, cell in enumerate(line):
                if cell == "#":
                    put(x + col, y + row, key)
                elif cell != ".":
                    put(x + col, y + row, cell)

    def cat(prefix, x, y, mirror):
        """Draw one cat. A mirrored cat sits on the left with its tail on the outside."""

        def at(gx):
            return x + (14 - gx if mirror else gx)

        for gy, row in enumerate(GRID):
            for gx, ch in enumerate(row):
                if ch == ".":
                    continue
                key = prefix + ("#" if ch == "E" else ch) if ch in "#DE" else ch
                put(at(gx), y + gy, key)
        for gx, gy in FACE["E"]:
            put(at(gx), y + gy, prefix + "E")
        for gx, gy in FACE["P"]:
            put(at(gx), y + gy, "P")

    cat("u", 1, 18, mirror=True)
    cat("o", 16, 18, mirror=False)
    sprite(HEART, 12, 9, "P")
    sprite(NOTE, 3, 9, "N")
    sprite(NOTES, 24, 7, "n")
    sprite(SPARKLE, 9, 3, "p")
    sprite(SPARKLE, 20, 2, "p")
    return grid


def main():
    grid = scene()
    (OUT / "jam.png").write_bytes(png(PALETTE, SIZE, grid))
    (OUT / "jam-512.png").write_bytes(png(PALETTE, 512, grid))


if __name__ == "__main__":
    main()
