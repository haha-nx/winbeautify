"""Print generated device icons as ASCII art, so the glyphs can be checked
without an image viewer.

    python tools/preview_icons.py [size]

Decodes the PNGs written by `make_icon.py` with zlib only (no Pillow) and
renders each pixel as a character: `#` for the white glyph, `+` for the plate
gradient, `.` for transparent.
"""

import os
import struct
import sys
import zlib


def read_png(path):
    """Return (width, height, RGBA bytes) for an 8-bit RGBA PNG."""
    with open(path, "rb") as fh:
        data = fh.read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos = 8
    width = height = None
    idat = bytearray()
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        tag = data[pos + 4 : pos + 8]
        payload = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if tag == b"IHDR":
            width, height, depth, color, _, _, _ = struct.unpack(">IIBBBBB", payload)
            assert depth == 8 and color == 6, "expected 8-bit RGBA"
        elif tag == b"IDAT":
            idat += payload
        elif tag == b"IEND":
            break

    raw = zlib.decompress(bytes(idat))
    stride = width * 4
    out = bytearray(width * height * 4)
    prev = bytearray(stride)
    pos = 0
    for y in range(height):
        filt = raw[pos]
        pos += 1
        line = bytearray(raw[pos : pos + stride])
        pos += stride
        if filt == 1:
            for i in range(4, stride):
                line[i] = (line[i] + line[i - 4]) & 0xFF
        elif filt == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif filt == 3:
            for i in range(stride):
                left = line[i - 4] if i >= 4 else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif filt == 4:
            for i in range(stride):
                a = line[i - 4] if i >= 4 else 0
                b = prev[i]
                c = prev[i - 4] if i >= 4 else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out[y * stride : (y + 1) * stride] = line
        prev = line
    return width, height, bytes(out)


def ascii_art(rgba, width, height):
    """`#` glyph, `+` plate, `.` transparent."""
    lines = []
    for y in range(height):
        row = []
        for x in range(width):
            i = (y * width + x) * 4
            r, g, b, a = rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]
            if a < 40:
                row.append(".")
            elif r > 200 and g > 200 and b > 200:
                row.append("#")
            else:
                row.append("+")
        lines.append("".join(row))
    return lines


def main():
    size = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    icons = os.path.join(root, "app", "icons")

    names = ["device-headphones", "device-speakers", "device-other"]
    renders = {}
    for name in names:
        w, h, rgba = read_png(os.path.join(icons, name + ".png"))
        renders[name] = (w, h, rgba)

    # Optionally downsample to mimic how the shell shrinks it for the tray.
    for name in names:
        w, h, rgba = renders[name]
        if size and size < w:
            step = w // size
            small = bytearray(size * size * 4)
            for y in range(size):
                for x in range(size):
                    acc = [0, 0, 0, 0]
                    for sy in range(step):
                        for sx in range(step):
                            i = ((y * step + sy) * w + (x * step + sx)) * 4
                            for c in range(4):
                                acc[c] += rgba[i + c]
                    o = (y * size + x) * 4
                    for c in range(4):
                        small[o + c] = acc[c] // (step * step)
            renders[name] = (size, size, bytes(small))

    # Print side by side, so the three states can be compared at a glance.
    for name in names:
        w, h, rgba = renders[name]
        print("\n=== %s (%dx%d) ===" % (name, w, h))
        for line in ascii_art(rgba, w, h):
            print("   " + line)

    # A crude identical-check: the three must not be pixel-identical.
    print()
    for a in range(len(names)):
        for b in range(a + 1, len(names)):
            na, nb = names[a], names[b]
            same = renders[na][2] == renders[nb][2]
            print("%s vs %s: %s" % (na, nb, "IDENTICAL (bug!)" if same else "differ"))


if __name__ == "__main__":
    main()
