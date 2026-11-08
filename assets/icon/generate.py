"""Generate the Meowtify icon files. Uses only the standard library.

Run from anywhere: python assets/icon/generate.py
"""

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).parent

# # body, D body shade, E eye, H headphones, S headphone highlight,
# W headphone accent, . transparent
GRID = """
................
................
..#.........#...
..##.......##...
..HHSSSSSSSHH...
.SSDDDDDDDDDSS.#
.WH#E#####E#HW.#
.WH#E#####E#HW.#
.HH#########HH.#
..D#########D..#
..#############.
..###########...
..##.##.##.##...
..DD.DD.DD.DD...
................
................
""".strip().splitlines()

# "All Songs" icon: a list of tracks beside a music note. One flat color on transparent.
ALL_SONGS_GRID = """
........##.
#######.###
........#.#
........#..
#######.#..
........#..
........#..
####.####..
.....####..
.....####..
####..##...
""".strip().splitlines()

HEADPHONES = (0x17, 0x17, 0x1C)
HIGHLIGHT = (0x3D, 0x3D, 0x48)
ACCENT = (0xFF, 0xFF, 0xFF)
# name: (body, body shade, eye)
VARIANTS = {
    "orange": ((0xF2, 0x9E, 0x4C), (0xD9, 0x82, 0x3A), (0x2B, 0x1D, 0x14)),
    "purple": ((0x9B, 0x7B, 0xF0), (0x7F, 0x62, 0xD6), (0x1E, 0x18, 0x33)),
    "white": ((0xF4, 0xEF, 0xE6), (0xD9, 0xD2, 0xC4), (0x24, 0x22, 0x20)),
}
ICO_SIZES = (16, 32, 48, 64, 128, 256)


def colors(body, shade, eye):
    return {"#": body, "D": shade, "E": eye, "H": HEADPHONES, "S": HIGHLIGHT, "W": ACCENT}


def png(palette, size, grid=GRID):
    """Encode a square grid as RGBA PNG. Size must be a multiple of the grid size."""
    assert size % len(grid) == 0
    scale = size // len(grid)
    rows = []
    for row in grid:
        line = b"\0"
        for ch in row:
            pixel = bytes(palette[ch]) + b"\xff" if ch != "." else b"\0\0\0\0"
            line += pixel * scale
        rows.extend([line] * scale)

    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
        + chunk(b"IEND", b"")
    )


def ico(palette):
    images = [png(palette, size) for size in ICO_SIZES]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries = b""
    for size, data in zip(ICO_SIZES, images):
        dim = 0 if size == 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return header + entries + b"".join(images)


def svg(palette):
    rects = []
    for y, row in enumerate(GRID):
        x = 0
        while x < 16:
            ch = row[x]
            run = 1
            while x + run < 16 and row[x + run] == ch:
                run += 1
            if ch != ".":
                r, g, b = palette[ch]
                rects.append(f'<rect x="{x}" y="{y}" width="{run}" height="1" fill="#{r:02X}{g:02X}{b:02X}"/>')
            x += run
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" shape-rendering="crispEdges">'
        + "".join(rects)
        + "</svg>\n"
    )


def main():
    assert len(GRID) == 16 and all(len(row) == 16 for row in GRID)
    for name, (body, shade, eye) in VARIANTS.items():
        palette = colors(body, shade, eye)
        (OUT / f"meowtify-{name}.svg").write_text(svg(palette), encoding="utf-8")
        (OUT / f"meowtify-{name}.ico").write_bytes(ico(palette))
        (OUT / f"meowtify-{name}-512.png").write_bytes(png(palette, 512))
    assert len(ALL_SONGS_GRID) == 11 and all(len(row) == 11 for row in ALL_SONGS_GRID)
    (OUT / "all-songs-black.png").write_bytes(png({"#": (0, 0, 0)}, 11, ALL_SONGS_GRID))
    (OUT / "all-songs-white.png").write_bytes(png({"#": (255, 255, 255)}, 11, ALL_SONGS_GRID))


if __name__ == "__main__":
    main()
