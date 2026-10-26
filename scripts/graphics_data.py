"""Generates the data of the graphics tests: PNG images of each kind PNG
has, each with the digest of the pixels it holds, and a small TrueType
font, alone and in a collection.

    python scripts/graphics_data.py

The images are made here, with Python's zlib, from pixels made here, so
that a decoder that reads them as their digests say reads them as this
script meant: every kind of color at every depth, palettes and their
transparency, transparent colors, interlacing, each of the five filters,
stored, fixed, and dynamic codes, data in several chunks, and chunks to
pass over; then images that are damaged. The font has a box for missing
characters, a space, glyphs of lines and of curves (one only off its
curve), glyphs made of others, moved and scaled, and a character beyond
the basic plane. Writes tests/programs/modules/graphics_data.sayon.
"""

import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODULE = ROOT / "tests" / "programs" / "modules" / "graphics_data.sayon"


def digest(pixels):
    """The tests' sum of the pixels' bytes, as `digest` in Sayon."""
    total, position = 0, 1
    for byte in pixels:
        total = (total + byte * position) % 1_000_000_007
        position = position % 9973 + 1
    return total


class Random:
    """A small generator of the same numbers every time."""

    def __init__(self, seed):
        self.state = seed

    def next(self, limit):
        self.state = (self.state * 1103515245 + 12345) % 2**31
        return (self.state >> 8) % limit


# ----- PNG ----------------------------------------------------------------------

def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}
ADAM7 = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]


def pack_row(samples, depth):
    """A row's samples as bytes, at their depth."""
    if depth == 16:
        return b"".join(struct.pack(">H", s) for s in samples)
    if depth == 8:
        return bytes(samples)
    out, bits, count = bytearray(), 0, 0
    for s in samples:
        bits = (bits << depth) | s
        count += depth
        if count == 8:
            out.append(bits)
            bits, count = 0, 0
    if count:
        out.append(bits << (8 - count))
    return bytes(out)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def filter_rows(rows, step, first_kind):
    """Filters each row with the next of the five filters in turn."""
    out = bytearray()
    above = bytes(len(rows[0])) if rows else b""
    for index, row in enumerate(rows):
        kind = (first_kind + index) % 5
        out.append(kind)
        for i, value in enumerate(row):
            left = row[i - step] if i >= step else 0
            up = above[i]
            corner = above[i - step] if i >= step else 0
            predicted = [0, left, up, (left + up) // 2, paeth(left, up, corner)][kind]
            out.append((value - predicted) % 256)
        above = row
    return bytes(out)


def png(width, height, color, depth, pixels, palette=None, transparent=None, interlace=False,
        level=6, strategy=zlib.Z_DEFAULT_STRATEGY, pieces=1, extra=b""):
    """A PNG image of `pixels`, each a tuple of its samples."""
    channels = CHANNELS[color]
    step = max(1, channels * depth // 8)
    passes = ADAM7 if interlace else [(0, 0, 1, 1)]
    raw = bytearray()
    for number, (x0, y0, dx, dy) in enumerate(passes):
        columns = range(x0, width, dx)
        rows = [pack_row([s for x in columns for s in pixels[y][x]], depth) for y in range(y0, height, dy)]
        if columns and rows:
            raw += filter_rows(rows, step, number)
    compressor = zlib.compressobj(level, zlib.DEFLATED, 15, 9, strategy)
    data = compressor.compress(bytes(raw)) + compressor.flush()
    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, depth, color, 0, 0, 1 if interlace else 0))
    out += extra
    if palette is not None:
        out += chunk(b"PLTE", bytes(v for entry in palette for v in entry))
    if transparent is not None:
        out += chunk(b"tRNS", transparent)
    size = -(-len(data) // pieces)
    for at in range(0, len(data), size):
        out += chunk(b"IDAT", data[at:at + size])
    out += chunk(b"IEND", b"")
    return out


def to8(value, depth):
    if depth == 16:
        return value >> 8
    return value * 255 // ((1 << depth) - 1)


def rgba(color, depth, pixel, palette=None, alphas=b"", transparent=None):
    """The red, green, blue, and alpha bytes a pixel decodes to."""
    if color == 3:
        index = pixel[0]
        red, green, blue = palette[index]
        alpha = alphas[index] if index < len(alphas) else 255
        return [red, green, blue, alpha]
    if color == 0:
        gray = to8(pixel[0], depth)
        alpha = 0 if transparent == pixel[0] else 255
        return [gray, gray, gray, alpha]
    if color == 2:
        alpha = 0 if transparent == tuple(pixel) else 255
        return [to8(v, depth) for v in pixel] + [alpha]
    if color == 4:
        gray = to8(pixel[0], depth)
        return [gray, gray, gray, to8(pixel[1], depth)]
    return [to8(v, depth) for v in pixel]


def image(name, width, height, color, depth, seed, palette_size=0, transparent=None, alphas=None,
          repetitive=False, **options):
    """A sample: an image of pixels made from `seed`, and its digest."""
    random = Random(seed)
    channels = CHANNELS[color]
    most = palette_size if color == 3 else 1 << depth
    pixels = []
    for y in range(height):
        row = []
        for x in range(width):
            if repetitive:
                pixel = tuple(((x // 4 + y // 3 + c) * 37) % most for c in range(channels))
            else:
                pixel = tuple(random.next(most) for _ in range(channels))
            row.append(pixel)
        pixels.append(row)
    # A pixel or two of the transparent color, where there is one.
    if transparent is not None:
        pixels[0][0] = transparent if isinstance(transparent, tuple) else (transparent,)
        pixels[height - 1][width - 1] = pixels[0][0]
    palette = None
    if color == 3:
        palette = [(random.next(256), random.next(256), random.next(256)) for _ in range(palette_size)]
    stored = None
    if alphas is not None:
        stored = bytes(alphas)
    elif transparent is not None:
        values = transparent if isinstance(transparent, tuple) else (transparent,)
        stored = b"".join(struct.pack(">H", v) for v in values)
    data = png(width, height, color, depth, pixels, palette, stored, **options)
    decoded = []
    key = transparent if isinstance(transparent, tuple) else transparent
    for row in pixels:
        for pixel in row:
            decoded += rgba(color, depth, pixel, palette, alphas or b"", key)
    return (name, data, digest(decoded))


def images():
    text = chunk(b"tEXt", b"Comment\x00made for the Sayon tests")
    samples = [
        image("gray 1-bit", 13, 5, 0, 1, 1),
        image("gray 2-bit", 7, 6, 0, 2, 2),
        image("gray 4-bit", 9, 4, 0, 4, 3),
        image("gray 8-bit, a transparent gray", 8, 8, 0, 8, 4, transparent=77),
        image("gray 16-bit", 5, 5, 0, 16, 5),
        image("color 8-bit, stored", 11, 7, 2, 8, 6, level=0),
        image("color 16-bit, a transparent color", 6, 4, 2, 16, 7, transparent=(1000, 2000, 3000)),
        image("color 8-bit, interlaced, 3 by 2", 3, 2, 2, 8, 8, interlace=True),
        image("palette 1-bit", 10, 3, 3, 1, 9, palette_size=2),
        image("palette 2-bit, interlaced, transparent", 9, 9, 3, 2, 10, palette_size=4,
              alphas=[0, 128], interlace=True),
        image("palette 4-bit", 17, 5, 3, 4, 11, palette_size=13),
        image("palette 8-bit, fixed codes", 16, 16, 3, 8, 12, palette_size=200,
              strategy=zlib.Z_FIXED),
        image("gray and alpha 8-bit", 6, 6, 4, 8, 13),
        image("gray and alpha 16-bit", 4, 3, 4, 16, 14),
        image("color and alpha 8-bit, interlaced, in pieces", 33, 17, 6, 8, 15, interlace=True,
              pieces=4, extra=text),
        image("color and alpha 16-bit", 3, 2, 6, 16, 16),
        image("color and alpha 8-bit, 1 by 1, interlaced", 1, 1, 6, 8, 17, interlace=True),
        image("color and alpha 8-bit, repeating", 40, 30, 6, 8, 18, repetitive=True, level=9),
    ]
    good = samples[0][1]
    damaged = bytearray(good)
    damaged[20] ^= 0x40
    out_of_palette = png(4, 1, 3, 4, [[(0,), (1,), (5,), (1,)]], palette=[(1, 2, 3), (4, 5, 6)])
    unknown = good[:33] + chunk(b"ZZZZ", b"strange") + good[33:]
    samples += [
        ("not an image", b"hello, world", -1),
        ("cut short", good[:40], -1),
        ("damaged", bytes(damaged), -1),
        ("a color its palette lacks", out_of_palette, -1),
        ("an unknown chunk it needs", unknown, -1),
    ]
    return samples


# ----- TrueType ---------------------------------------------------------------

def glyph_simple(contours):
    """A simple glyph: contours of points (x, y, on the curve)."""
    points = [p for contour in contours for p in contour]
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    out = struct.pack(">hhhhh", len(contours), min(xs), min(ys), max(xs), max(ys))
    end = -1
    for contour in contours:
        end += len(contour)
        out += struct.pack(">H", end)
    out += struct.pack(">H", 0)
    flags, xdata, ydata = [], b"", b""
    last_x = last_y = 0
    for x, y, on in points:
        flag = 1 if on else 0
        for value, last, short, same in ((x, last_x, 2, 16), (y, last_y, 4, 32)):
            change = value - last
            data = b""
            if change == 0:
                flag |= same
            elif -255 <= change <= 255:
                flag |= short | (same if change > 0 else 0)
                data = bytes([abs(change)])
            else:
                data = struct.pack(">h", change)
            if short == 2:
                xdata += data
            else:
                ydata += data
        flags.append(flag)
        last_x, last_y = x, y
    # Runs of the same flag, repeated.
    packed = bytearray()
    index = 0
    while index < len(flags):
        run = 1
        while index + run < len(flags) and flags[index + run] == flags[index] and run < 255:
            run += 1
        if run > 1:
            packed += bytes([flags[index] | 8, run - 1])
        else:
            packed.append(flags[index])
        index += run
    return out + bytes(packed) + xdata + ydata


def glyph_composite(bounds, components):
    """A glyph of others: (glyph, dx, dy, scale or None)."""
    out = struct.pack(">hhhhh", -1, *bounds)
    for index, (glyph, dx, dy, scale) in enumerate(components):
        words = not (-128 <= dx <= 127 and -128 <= dy <= 127)
        flags = 2 | (1 if words else 0) | (8 if scale is not None else 0)
        if index + 1 < len(components):
            flags |= 0x20
        out += struct.pack(">HH", flags, glyph)
        out += struct.pack(">hh", dx, dy) if words else struct.pack(">bb", dx, dy)
        if scale is not None:
            out += struct.pack(">h", round(scale * 16384))
    return out


def rectangle(x0, y0, x1, y1, clockwise=True):
    points = [(x0, y0, True), (x0, y1, True), (x1, y1, True), (x1, y0, True)]
    return points if clockwise else points[::-1]


def circle(cx, cy, r, all_off=False, clockwise=True):
    """A circle of quadratic curves: points on it and off it in turn, or
    eight off it alone, between each two of which is one on it."""
    points = []
    for k in range(8):
        angle = k * 45
        if all_off:
            # The points off the curve of a circle drawn by eight curves.
            reach = r / 0.9238795
            points.append((cx + round(reach * cos(angle + 22.5)), cy + round(reach * sin(angle + 22.5)), False))
        else:
            reach = r if k % 2 == 0 else r * 1.4142136
            points.append((cx + round(reach * cos(angle)), cy + round(reach * sin(angle)), k % 2 == 0))
    return points[::-1] if clockwise else points


def cos(degrees):
    import math
    return math.cos(math.radians(degrees))


def sin(degrees):
    import math
    return math.sin(math.radians(degrees))


GLYPHS = [
    # The box for missing characters, a hole in it.
    (500, glyph_simple([rectangle(50, 0, 450, 700), rectangle(100, 50, 400, 650, clockwise=False)])),
    (250, b""),
    # "A": its outline and the hole in it.
    (600, glyph_simple([
        [(0, 0, True), (260, 700, True), (340, 700, True), (600, 0, True), (520, 0, True),
         (430, 250, True), (170, 250, True), (80, 0, True)],
        [(200, 330, True), (400, 330, True), (300, 600, True)],
    ])),
    # "o": an outer curve only off it, and an inner one on and off.
    (500, glyph_simple([circle(250, 250, 230, all_off=True), circle(250, 250, 150, clockwise=False)])),
    # "l"
    (220, glyph_simple([rectangle(60, 0, 160, 750)])),
    # The ring of "Å", about its origin.
    (0, glyph_simple([circle(0, 0, 80), circle(0, 0, 40, clockwise=False)])),
    # "Å": "A" and the ring above it, moved.
    (600, glyph_composite((0, 0, 600, 900), [(2, 0, 0, None), (5, 300, 820, None)])),
    # "!": "l" at half its size, moved a little.
    (300, glyph_composite((80, 10, 130, 385), [(4, 50, 10, 0.5)])),
    # U+1F600, beyond the basic plane: a diamond. It takes the last
    # advance of its own.
    (None, glyph_simple([[(250, 0, True), (0, 350, True), (250, 700, True), (500, 350, True)]])),
]
CHARACTERS = {" ": 1, "!": 7, "A": 2, "l": 4, "o": 3, "Å": 6, "\U0001F600": 8}


def cmap_segments():
    """Format 4: a segment for each character of the basic plane, "A"'s
    through the array of glyphs, then the one that ends the table."""
    codes = sorted(ord(c) for c in CHARACTERS if ord(c) <= 0xFFFF)
    segments = [(code, code, CHARACTERS[chr(code)]) for code in codes] + [(0xFFFF, 0xFFFF, 0)]
    count = len(segments)
    ends = b"".join(struct.pack(">H", s[1]) for s in segments)
    starts = b"".join(struct.pack(">H", s[0]) for s in segments)
    deltas, ranges, array = b"", b"", b""
    for index, (start, _, glyph) in enumerate(segments):
        if start == ord("A"):
            deltas += struct.pack(">h", 0)
            # From this word to the array after the last segment's.
            ranges += struct.pack(">H", 2 * (count - index) + len(array))
            array += struct.pack(">H", glyph)
        else:
            deltas += struct.pack(">H", (glyph - start) % 65536 if start != 0xFFFF else 1)
            ranges += struct.pack(">H", 0)
    body = ends + b"\x00\x00" + starts + deltas + ranges + array
    return struct.pack(">HHHHHHH", 4, 14 + len(body), 0, 2 * count, 0, 0, 0) + body


def cmap_groups():
    groups = sorted((ord(c), CHARACTERS[c]) for c in CHARACTERS)
    body = b"".join(struct.pack(">III", code, code, glyph) for code, glyph in groups)
    return struct.pack(">HHIII", 12, 0, 16 + len(body), 0, len(groups)) + body


def cmap(full):
    subtables = [(3, 1, cmap_segments())] + ([(3, 10, cmap_groups())] if full else [])
    out = struct.pack(">HH", 0, len(subtables))
    offset = 4 + 8 * len(subtables)
    data = b""
    for platform, encoding, table in subtables:
        out += struct.pack(">HHI", platform, encoding, offset + len(data))
        data += table
    return out + data


def name_table():
    records = [(3, 1, 0x409, 1, "Sayon Test"), (3, 1, 0x409, 2, "Regular"),
               (3, 1, 0x409, 4, "Sayon Test Regular"), (1, 0, 0, 4, "Not This Name")]
    strings, entries = b"", b""
    for platform, encoding, language, number, text in records:
        data = text.encode("latin-1") if platform == 1 else text.encode("utf-16-be")
        entries += struct.pack(">HHHHHH", platform, encoding, language, number, len(data), len(strings))
        strings += data
    return struct.pack(">HHH", 0, len(records), 6 + len(entries)) + entries + strings


def font(long_offsets, full_cmap):
    glyf, loca = b"", []
    for _, data in GLYPHS:
        loca.append(len(glyf))
        glyf += data + bytes(-len(data) % 4)
    loca.append(len(glyf))
    advances = [advance for advance, _ in GLYPHS if advance is not None]
    head = bytearray(54)
    struct.pack_into(">I", head, 0, 0x00010000)
    struct.pack_into(">H", head, 18, 1000)
    struct.pack_into(">h", head, 50, 1 if long_offsets else 0)
    hhea = bytearray(36)
    struct.pack_into(">Ihhh", hhea, 0, 0x00010000, 800, -200, 90)
    struct.pack_into(">H", hhea, 34, len(advances))
    tables = {
        b"cmap": cmap(full_cmap),
        b"glyf": glyf,
        b"head": bytes(head),
        b"hhea": bytes(hhea),
        b"hmtx": b"".join(struct.pack(">Hh", a, 0) for a in advances),
        b"loca": b"".join(struct.pack(">I" if long_offsets else ">H", o if long_offsets else o // 2) for o in loca),
        b"maxp": struct.pack(">IH", 0x00005000, len(GLYPHS)),
        b"name": name_table(),
    }
    return tables


def sfnt(tables, start):
    """The font's bytes, starting `start` bytes into the file."""
    out = struct.pack(">IHHHH", 0x00010000, len(tables), 0, 0, 0)
    offset = start + 12 + 16 * len(tables)
    body = b""
    for tag in sorted(tables):
        data = tables[tag]
        out += tag + struct.pack(">III", 0, offset + len(body), len(data))
        body += data + bytes(-len(data) % 4)
    return out + body


def collection():
    """A collection of one font: the same glyphs, with format 4 alone and
    16-bit offsets."""
    header = b"ttcf" + struct.pack(">HHII", 1, 0, 1, 16)
    return header + sfnt(font(False, False), len(header))


# ----- the module ---------------------------------------------------------------

def main():
    lines = [
        "# Generated by scripts/graphics_data.py: PNG images of each kind, with",
        "# the digest of the pixels each holds (-1 for one that is damaged), and",
        "# a small TrueType font, alone and in a collection.",
        "strings = import(\"standard/strings.sayon\")",
        "",
        "type Sample = [name: string, hex: string, digest: integer]",
        "",
        "function images() -> Array<Sample>",
        "    samples: Array<Sample> = []",
    ]
    for name, data, expected in images():
        lines.append(f"    push(samples, Sample(name = \"{name}\", hex = \"{data.hex()}\", digest = {expected}))")
    lines += [
        "    return samples",
        "",
        "function font() -> Array<byte>",
        f"    return unhex(\"{sfnt(font(True, True), 0).hex()}\")",
        "",
        "function collection() -> Array<byte>",
        f"    return unhex(\"{collection().hex()}\")",
        "",
        "# The bytes hexadecimal digits spell, two digits a byte.",
        "function unhex(text: string) -> Array<byte>",
        "    digits = strings.bytes(text)",
        "    out: Array<byte> = []",
        "    index = 0",
        "    while index + 1 < length(digits)",
        "        push(out, unsigned8(digit(integer(digits[index])) * 16 + digit(integer(digits[index + 1]))))",
        "        index += 2",
        "    return out",
        "",
        "private function digit(code: integer) -> integer",
        "    if code >= 97",
        "        return code - 87",
        "    return code - 48",
        "",
    ]
    MODULE.write_bytes("\n".join(lines).encode("utf-8"))


if __name__ == "__main__":
    main()
