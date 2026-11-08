"""Generate animated GIFs of the Meowtify mascot. Uses only the standard library.

The cat comes from the grid in generate.py, so the GIFs follow any change to the icon.

Run from anywhere: python assets/icon/animate.py
"""

import math
import struct
from pathlib import Path

from generate import ACCENT, GRID, HEADPHONES, HIGHLIGHT, VARIANTS

OUT = Path(__file__).parent / "gif"
SIZE = 24  # Canvas size in art pixels. The cat is 16 wide, which leaves room for effects.
SCALE = 10  # Screen pixels per art pixel.
CAT_X, CAT_Y = 4, 8  # Default position of the cat grid on the canvas.

BODY, SHADE, EYE = VARIANTS["orange"]
PINK = (0xF2, 0x6D, 0x8F)
NOTE = (0x9B, 0x7B, 0xF0)
SNOOZE = (0x5C, 0x9D, 0xE0)
SHADOW = (0x8A, 0x8A, 0x96)
LAPTOP = (0x1C, 0x1C, 0x22)
LAPTOP_EDGE = (0x4A, 0x4A, 0x58)
LAPTOP_BASE = (0x2E, 0x2E, 0x38)

# Index 0 is the transparent color. GIF color tables hold a power of two entries.
PALETTE = [(0, 0, 0), BODY, SHADE, EYE, HEADPHONES, HIGHLIGHT, ACCENT, PINK, NOTE, SNOOZE, SHADOW, LAPTOP, LAPTOP_EDGE, LAPTOP_BASE]
PALETTE += [(0, 0, 0)] * (16 - len(PALETTE))
GRID_COLORS = {"#": 1, "D": 2, "E": 1, "H": 4, "S": 5, "W": 6}  # Eyes are drawn separately.
I_BODY, I_SHADE, I_EYE, I_ACCENT, I_PINK, I_NOTE, I_SNOOZE, I_SHADOW = 1, 2, 3, 6, 7, 8, 9, 10
I_LAPTOP, I_LAPTOP_EDGE, I_LAPTOP_BASE = 11, 12, 13
I_FRAME = 4  # Glasses frames share the headphones' color.

LEG_COLUMNS = ((2, 3), (5, 6), (8, 9), (11, 12))
# Tail poses, in grid coordinates. Every pose joins the body at the same two pixels.
TAIL_BASE = [(13, 10), (14, 10)]
TAILS = [
    [(15, 9), (15, 8), (15, 7), (15, 6), (15, 5)],  # Up, as in the icon.
    [(15, 9), (15, 8), (15, 7), (16, 6), (16, 5)],  # Tip out.
    [(15, 9), (15, 8), (15, 7), (14, 6), (14, 5)],  # Tip in.
    [(15, 11), (16, 11)],  # Resting.
    [(15, 11), (16, 10)],  # Resting, tip twitching.
]
# Ear poses: icon, flicked outward, drooped.
EARS_LEFT = [[(2, 2), (2, 3), (3, 3)], [(1, 2), (2, 3), (3, 3)], [(2, 3), (3, 3)]]
EARS_RIGHT = [[(12, 2), (11, 3), (12, 3)], [(13, 2), (11, 3), (12, 3)], [(11, 3), (12, 3)]]
EYES = {
    "open": [(4, 6), (4, 7), (10, 6), (10, 7)],
    "left": [(3, 6), (3, 7), (9, 6), (9, 7)],
    "right": [(5, 6), (5, 7), (11, 6), (11, 7)],
    "closed": [(3, 7), (4, 7), (5, 7), (9, 7), (10, 7), (11, 7)],
    "happy": [(3, 7), (4, 6), (5, 7), (9, 7), (10, 6), (11, 7)],
    "squint": [(4, 7), (10, 7)],
}

SPRITE_NOTE = [".##", ".#.", ".#.", "##.", "##."]
SPRITE_NOTES = [".####", ".#..#", ".#..#", "##.##", "##.##"]
SPRITE_HEART_S = ["#.#", "###", ".#."]
SPRITE_HEART_L = [".#.#.", "#####", "#####", ".###.", "..#.."]
SPRITE_Z = [["##.", ".#.", ".##"], ["####", "..#.", ".#..", "####"], ["#####", "...#.", "..#..", ".#...", "#####"]]
LETTERS = {
    "M": ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
    "E": ["###", "#..", "##.", "#..", "###"],
    "O": ["###", "#.#", "#.#", "#.#", "###"],
    "W": ["#...#", "#...#", "#.#.#", "##.##", "#...#"],
}


def blank():
    return [[0] * SIZE for _ in range(SIZE)]


def put(canvas, x, y, color):
    if 0 <= x < SIZE and 0 <= y < SIZE:
        canvas[y][x] = color


def sprite(canvas, art, x, y, color):
    for row, line in enumerate(art):
        for col, ch in enumerate(line):
            if ch == "#":
                put(canvas, x + col, y + row, color)


def cat(canvas, x=CAT_X, y=CAT_Y, head=0, eyes="open", mouth=None, blush=False, tail=0, ears=(0, 0), legs=(0, 0, 0, 0)):
    """Draw the cat. `head` moves the head down in art pixels. A leg lift of 1 hides the paw row."""
    for gx, gy in TAIL_BASE + TAILS[tail]:
        put(canvas, x + gx, y + gy, I_BODY)
    for columns, lift in zip(LEG_COLUMNS, legs):
        for gx in columns:
            if lift:
                put(canvas, x + gx, y + 12, I_SHADE)
            else:
                put(canvas, x + gx, y + 12, I_BODY)
                put(canvas, x + gx, y + 13, I_SHADE)
    for gy in (10, 11):
        for gx in range(2, 13):
            put(canvas, x + gx, y + gy, I_BODY)
    # A stretched cat needs body under the raised head.
    for gy in range(10 + head, 10):
        for gx in range(2, 13):
            put(canvas, x + gx, y + gy, I_BODY)
    for gy in range(4, 10):
        for gx in range(15):
            ch = GRID[gy][gx]
            if ch != ".":
                put(canvas, x + gx, y + gy + head, GRID_COLORS[ch])
    for gx, gy in EARS_LEFT[ears[0]] + EARS_RIGHT[ears[1]]:
        put(canvas, x + gx, y + gy + head, I_BODY)
    for gx, gy in EYES[eyes]:
        put(canvas, x + gx, y + gy + head, I_EYE)
    if blush:
        for gx in (3, 4, 10, 11):
            put(canvas, x + gx, y + 8 + head, I_PINK)
    if mouth == "smile":
        # Row 9, not row 8: higher up, the smile joins the happy eyes into one zigzag.
        for gx, gy in ((6, 9), (7, 10), (8, 9)):
            put(canvas, x + gx, y + gy + head, I_EYE)
    elif mouth == "open":
        for gx in (6, 7, 8):
            put(canvas, x + gx, y + 9 + head, I_EYE)
            put(canvas, x + gx, y + 10 + head, I_EYE)
        put(canvas, x + 7, y + 10 + head, I_PINK)


def float_up(canvas, frame, period, phase, x, y, rise, art_for, color):
    """Draw a particle that rises and sways. It loops when `period` divides the frame count."""
    age = ((frame + phase) % period) / period
    if age > 0.85:
        return
    sway = round(math.sin(age * math.tau) * 1.4)
    sprite(canvas, art_for(age), x + sway, y - round(age * rise), color)


def idle():
    frames = []
    for i in range(30):
        c = blank()
        cat(
            c,
            head=1 if i % 15 >= 9 else 0,
            eyes="closed" if i in (7, 8, 24) else "open",
            tail=(0, 1, 1, 0, 2, 2)[i % 6],
            ears=(1, 0) if i in (18, 19) else (0, 0),
        )
        frames.append((c, 11))
    return frames


def vibe():
    frames = []
    for i in range(24):
        c = blank()
        float_up(c, i, 24, 0, 1, 12, 12, lambda age: SPRITE_NOTE, I_NOTE)
        float_up(c, i, 24, 8, 19, 9, 9, lambda age: SPRITE_NOTES, I_NOTE)
        float_up(c, i, 24, 16, 10, 7, 7, lambda age: SPRITE_NOTE, I_NOTE)
        down = i % 4 in (1, 2)
        cat(
            c,
            head=1 if down else 0,
            eyes="closed" if 8 <= i < 20 else "happy",
            mouth="smile",
            tail=(1, 0, 2, 0)[i % 4],
            ears=(1, 1) if down else (0, 0),
        )
        frames.append((c, 9))
    return frames


def dance():
    heights = (0, 0, 1, 3, 4, 4, 3, 1, 0, 0, 0, 0)
    frames = []
    for i in range(24):
        c = blank()
        step = i % 12
        height = heights[step]
        # The cat lands on one side, then hops back to the other.
        progress = min(step, 8) / 8
        x = CAT_X - 2 + round(4 * (progress if i < 12 else 1 - progress))
        width = 12 - height * 2
        for sx in range(x + 8 - width // 2, x + 8 + width // 2):
            put(c, sx, 22, I_SHADOW)
        squash = step in (0, 1, 8, 9)
        cat(
            c,
            x=x,
            y=CAT_Y - height,
            head=1 if squash else (-1 if step in (2, 3) else 0),
            eyes="happy" if height else "open",
            mouth="smile" if height else None,
            tail=(1 if i < 12 else 2) if height else 0,
            ears=(1, 1) if height >= 3 else (0, 0),
            legs=(1, 1, 1, 1) if squash else (0, 0, 0, 0),
        )
        frames.append((c, 7))
    return frames


def sleep():
    frames = []
    for i in range(33):
        c = blank()
        # The Zs appear one by one, small to large, then all clear together.
        for stage, (art, x, y) in enumerate(zip(SPRITE_Z, (12, 15, 19), (8, 5, 0))):
            if stage * 8 <= i < 26:
                sprite(c, art, x, y, I_SNOOZE)
        cat(
            c,
            y=CAT_Y + 1,
            head=1 if i % 11 < 6 else 0,
            eyes="closed",
            tail=4 if i in (15, 16) else 3,
            ears=(2, 2),
            legs=(1, 1, 1, 1),
        )
        frames.append((c, 14))
    return frames


def love():
    frames = []
    for i in range(24):
        c = blank()
        small_then_big = lambda age: SPRITE_HEART_S if age < 0.3 else SPRITE_HEART_L
        float_up(c, i, 24, 0, 1, 12, 12, small_then_big, I_PINK)
        float_up(c, i, 24, 8, 18, 8, 8, small_then_big, I_PINK)
        float_up(c, i, 24, 16, 9, 7, 7, small_then_big, I_PINK)
        cat(
            c,
            head=1 if i % 8 >= 5 else 0,
            eyes="happy",
            mouth="smile",
            blush=True,
            tail=(0, 1, 1, 0, 2, 2)[i % 6],
            ears=(1, 0) if i % 12 in (0, 1) else ((0, 1) if i % 12 in (6, 7) else (0, 0)),
        )
        frames.append((c, 9))
    return frames


def bubble(canvas, text):
    """Draw the speech bubble and as much of the word as `text` holds."""
    for y in range(0, 9):
        for x in range(0, 23):
            corner = x in (0, 22) and y in (0, 8)
            edge = x in (0, 22) or y in (0, 8)
            if not corner:
                put(canvas, x, y, I_EYE if edge else I_ACCENT)
    put(canvas, 11, 9, I_EYE)
    x = 2
    for letter in text:
        sprite(canvas, LETTERS[letter], x, 2, I_EYE)
        x += len(LETTERS[letter][0]) + 1


def meow():
    frames = []

    def add(delay, text=None, **pose):
        c = blank()
        cat(c, **pose)
        if text is not None:
            bubble(c, text)
        frames.append((c, delay))

    add(70)
    add(10, eyes="closed")
    add(40)
    add(14, head=1, eyes="closed", legs=(1, 1, 1, 1))
    for text in ("M", "ME", "MEO", "MEOW"):
        add(7, text=text, head=-1, eyes="closed", mouth="open", ears=(1, 1), tail=1)
    for i in range(8):
        add(8, text="MEOW", head=-1 if i % 4 < 2 else 0, eyes="closed", mouth="open", ears=(1, 1) if i % 2 else (0, 0), tail=2 if i % 2 else 1)
    add(30, text="MEOW", eyes="happy", mouth="smile")
    add(50, eyes="happy", mouth="smile")
    return frames


def glasses(canvas, x=CAT_X, y=CAT_Y, head=0):
    """Draw half-rim glasses, black like the headphones. Open sides keep the eyes and headphones apart."""
    for left in (3, 9):
        for gx in range(left, left + 3):
            put(canvas, x + gx, y + 5 + head, I_FRAME)
            put(canvas, x + gx, y + 8 + head, I_FRAME)
    for gx in (6, 7, 8):
        put(canvas, x + gx, y + 6 + head, I_FRAME)


CODE_BITS = {"<": ["..#", ".#.", "..#"], ">": ["#..", ".#.", "#.."], "/": ["..#", ".#.", "#.."]}
# Which paw is down on each frame, as (left, right). Uneven, so it reads as typing.
TYPING_PAWS = [(1, 0), (0, 1), (1, 0), (1, 0), (0, 1), (0, 0), (1, 0), (0, 1),
               (0, 1), (1, 0), (0, 1), (1, 1), (1, 0), (0, 1), (0, 1), (1, 0),
               (1, 0), (0, 0), (0, 1), (1, 0), (0, 1), (1, 0), (1, 1), (0, 1)]


def typing():
    frames = []
    for i in range(24):
        c = blank()
        for phase, bit, bx in ((0, "<", 1), (8, "/", 19), (16, ">", 2)):
            float_up(c, i, 24, phase, bx, 15, 9, lambda age, bit=bit: CODE_BITS[bit], I_NOTE)
        head = 1 if i % 8 in (5, 6) else 0
        cat(
            c,
            head=head,
            eyes="squint" if i in (10, 11) else "open",
            tail=3 if i % 12 < 6 else 4,
            ears=(1, 0) if i in (17, 18) else (0, 0),
        )
        glasses(c, head=head)
        # The laptop, seen from behind its lid. It hides the cat's body and legs.
        for ly in range(CAT_Y + 10, CAT_Y + 14):
            for lx in range(CAT_X + 1, CAT_X + 14):
                put(c, lx, ly, I_LAPTOP_EDGE if ly == CAT_Y + 10 else I_LAPTOP)
        for lx in range(CAT_X, CAT_X + 15):
            put(c, lx, CAT_Y + 14, I_LAPTOP_BASE)
        left, right = TYPING_PAWS[i]
        for down, columns in ((left, (8, 9)), (right, (13, 14))):
            if down:
                for gx in columns:
                    put(c, CAT_X + gx - 4 + 4, CAT_Y + 10, I_BODY)
        frames.append((c, 6))
    return frames


def peek():
    """The cat rises from below the frame, looks around, and ducks back down."""
    frames = []

    def add(delay, y, **pose):
        c = blank()
        cat(c, y=y, **pose)
        frames.append((c, delay))

    add(70, 24)
    for y in (20, 16, 14):
        add(8, y)
    add(40, 14)
    add(40, 14, eyes="left", tail=2)
    add(40, 14, eyes="right", tail=1)
    add(10, 14, eyes="closed")
    add(12, 14, ears=(1, 1))
    add(12, 14)
    add(12, 14, ears=(1, 1))
    add(40, 14, eyes="happy")
    for y in (16, 20):
        add(6, y)
    # Crop the empty top half, because the cat never reaches it.
    return [(c[12:], delay) for c, delay in frames]


def lzw(pixels, min_code_size):
    """Encode color indexes with the LZW variant that GIF requires."""
    clear = 1 << min_code_size
    end = clear + 1
    output = bytearray()
    buffer = bits = 0

    def reset():
        return {}, end + 1, min_code_size + 1

    table, next_code, size = reset()

    def emit(code):
        nonlocal buffer, bits, size
        buffer |= code << bits
        bits += size
        while bits >= 8:
            output.append(buffer & 0xFF)
            buffer >>= 8
            bits -= 8
        # The decoder adds its entry one code later, so grow before this entry exists.
        if next_code == 1 << size and size < 12:
            size += 1

    emit(clear)
    prefix = pixels[0]
    for pixel in pixels[1:]:
        key = (prefix, pixel)
        if key in table:
            prefix = table[key]
            continue
        emit(prefix)
        if next_code < 4096:
            table[key] = next_code
            next_code += 1
        else:
            emit(clear)
            table, next_code, size = reset()
        prefix = pixel
    emit(prefix)
    emit(end)
    if bits:
        output.append(buffer & 0xFF)
    return bytes(output)


def encode(frames):
    """Encode (canvas, delay in centiseconds) pairs as a looping GIF with transparency."""
    # Merge repeated frames, so a hold costs one frame.
    merged = []
    for canvas, delay in frames:
        if merged and merged[-1][0] == canvas:
            merged[-1][1] += delay
        else:
            merged.append([canvas, delay])

    height = len(merged[0][0]) * SCALE
    width = len(merged[0][0][0]) * SCALE
    out = bytearray(b"GIF89a")
    # 0xF3: global color table present, 16 entries.
    out += struct.pack("<HHBBB", width, height, 0xF3, 0, 0)
    for color in PALETTE:
        out += bytes(color)
    out += b"\x21\xFF\x0BNETSCAPE2.0\x03\x01\x00\x00\x00"  # Loop forever.
    for canvas, delay in merged:
        # 0x09: clear to transparent after the frame, index 0 is transparent.
        out += b"\x21\xF9\x04\x09" + struct.pack("<H", delay) + b"\x00\x00"
        out += b"\x2C" + struct.pack("<HHHHB", 0, 0, width, height, 0)
        out += b"\x04"  # Minimum LZW code size.
        pixels = []
        for row in canvas:
            line = [color for color in row for _ in range(SCALE)]
            pixels.extend(line * SCALE)
        data = lzw(pixels, 4)
        for start in range(0, len(data), 255):
            block = data[start : start + 255]
            out += bytes([len(block)]) + block
        out += b"\x00"
    out += b"\x3B"
    return bytes(out)


def main():
    OUT.mkdir(exist_ok=True)
    for animation in (idle, vibe, dance, sleep, love, meow, peek, typing):
        (OUT / f"meowtify-{animation.__name__}.gif").write_bytes(encode(animation()))


if __name__ == "__main__":
    main()
