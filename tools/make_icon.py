"""Generate the WinBeautify app icon (PNG + multi-size ICO).

Pure stdlib: the icon is drawn procedurally and encoded with zlib, so this does
not need Pillow. Run from the repository root:

    python tools/make_icon.py

Writes `app/icons/icon.png` and `app/icons/icon.ico`.
"""

import math
import os
import struct
import zlib

# Brand gradient: the same indigo -> violet the UI accent uses.
TOP = (0x6C, 0x8C, 0xFF)
BOTTOM = (0xA5, 0x6C, 0xFF)


def lerp(a, b, t):
    return a + (b - a) * t


def rounded_rect_coverage(x, y, w, h, radius, samples=3):
    """Anti-aliased coverage of a rounded rectangle at pixel (x, y)."""
    hits = 0
    for sy in range(samples):
        for sx in range(samples):
            px = x + (sx + 0.5) / samples
            py = y + (sy + 0.5) / samples
            cx = min(max(px, radius), w - radius)
            cy = min(max(py, radius), h - radius)
            dx = px - cx
            dy = py - cy
            if dx * dx + dy * dy <= radius * radius:
                hits += 1
    return hits / (samples * samples)


def render(size):
    """Return RGBA bytes for a `size` x `size` icon."""
    ss = 4  # supersample factor for the inner artwork
    big = size * ss
    canvas = bytearray(big * big * 4)

    pad = big * 0.055
    body = big - 2 * pad
    radius = body * 0.235

    # --- rounded gradient plate ------------------------------------------
    for y in range(big):
        t = y / max(1, big - 1)
        r = lerp(TOP[0], BOTTOM[0], t)
        g = lerp(TOP[1], BOTTOM[1], t)
        b = lerp(TOP[2], BOTTOM[2], t)
        row = y * big * 4
        for x in range(big):
            cov = rounded_rect_coverage(x - pad, y - pad, body, body, radius)
            if cov <= 0:
                continue
            i = row + x * 4
            canvas[i] = int(r)
            canvas[i + 1] = int(g)
            canvas[i + 2] = int(b)
            canvas[i + 3] = int(255 * cov)

    # --- taskbar motif: three translucent bars + a bright indicator ------
    # Layered over the lower third of the plate, it reads as "taskbar" at 16px
    # and as a stack of panels at 256px.
    bar_h = body * 0.085
    gap = body * 0.062
    base_y = pad + body * 0.30
    bar_x = pad + body * 0.155
    bar_w = body * 0.69
    bar_radius = bar_h / 2

    bars = [
        (0.0, 0.34),   # dimmest, top
        (1.0, 0.52),
        (2.0, 0.92),   # the "active" bar
    ]
    for index, alpha in bars:
        top = base_y + index * (bar_h + gap)
        for y in range(int(top), min(big, int(top + bar_h) + 2)):
            for x in range(int(bar_x), min(big, int(bar_x + bar_w) + 2)):
                cov = rounded_rect_coverage(
                    x - bar_x, y - top, bar_w, bar_h, bar_radius
                )
                if cov <= 0:
                    continue
                i = (y * big + x) * 4
                if canvas[i + 3] == 0:
                    continue
                a = alpha * cov
                for c in range(3):
                    canvas[i + c] = int(canvas[i + c] * (1 - a) + 255 * a)
                canvas[i + 3] = max(canvas[i + 3], int(255 * a))

    # --- a small accent dot in the active bar (the "audio component") ----
    dot_r = bar_h * 0.34
    dot_cx = bar_x + bar_w - bar_h * 0.62
    dot_cy = base_y + 2 * (bar_h + gap) + bar_h / 2
    for y in range(int(dot_cy - dot_r - 2), min(big, int(dot_cy + dot_r + 2))):
        for x in range(int(dot_cx - dot_r - 2), min(big, int(dot_cx + dot_r + 2))):
            d = math.hypot(x + 0.5 - dot_cx, y + 0.5 - dot_cy)
            cov = max(0.0, min(1.0, dot_r - d + 0.5))
            if cov <= 0:
                continue
            i = (y * big + x) * 4
            # Punch it out of the bar rather than painting over it.
            canvas[i + 3] = int(canvas[i + 3] * (1 - cov))
            canvas[i] = int(canvas[i] * (1 - cov) + 0x14 * cov)
            canvas[i + 1] = int(canvas[i + 1] * (1 - cov) + 0x16 * cov)
            canvas[i + 2] = int(canvas[i + 2] * (1 - cov) + 0x1C * cov)

    # --- downsample -------------------------------------------------------
    out = bytearray(size * size * 4)
    area = ss * ss
    for y in range(size):
        for x in range(size):
            r = g = b = a = 0
            for sy in range(ss):
                for sx in range(ss):
                    i = ((y * ss + sy) * big + (x * ss + sx)) * 4
                    r += canvas[i]
                    g += canvas[i + 1]
                    b += canvas[i + 2]
                    a += canvas[i + 3]
            o = (y * size + x) * 4
            out[o] = r // area
            out[o + 1] = g // area
            out[o + 2] = b // area
            out[o + 3] = a // area
    return bytes(out)


def encode_png(rgba, size):
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)  # filter type 0 (None)
        raw += rgba[y * stride:(y + 1) * stride]

    def chunk(tag, data):
        payload = tag + data
        return (
            struct.pack(">I", len(data))
            + payload
            + struct.pack(">I", zlib.crc32(payload) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def encode_ico(pngs):
    """Pack PNG-encoded images into an ICO container.

    Vista and later read PNG-compressed entries directly, which keeps the file
    small and avoids hand-writing BMP masks.
    """
    count = len(pngs)
    header = struct.pack("<HHH", 0, 1, count)
    offset = 6 + 16 * count
    entries = b""
    for size, data in pngs:
        entries += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,
            0 if size >= 256 else size,
            0,
            0,
            1,
            32,
            len(data),
            offset,
        )
        offset += len(data)
    return header + entries + b"".join(data for _, data in pngs)


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    icons = os.path.join(root, "app", "icons")
    os.makedirs(icons, exist_ok=True)

    sizes = [16, 24, 32, 48, 64, 128, 256]
    encoded = []
    for size in sizes:
        png = encode_png(render(size), size)
        encoded.append((size, png))
        if size == 256:
            with open(os.path.join(icons, "icon.png"), "wb") as fh:
                fh.write(png)
        # Tauri's config wants a square PNG per size for the bundle.
        with open(os.path.join(icons, f"{size}x{size}.png"), "wb") as fh:
            fh.write(png)

    with open(os.path.join(icons, "icon.ico"), "wb") as fh:
        fh.write(encode_ico(encoded))

    print("wrote", icons)


if __name__ == "__main__":
    main()
