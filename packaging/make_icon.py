#!/usr/bin/env python3
"""Render the edittr app icon with no dependencies (signed-distance shapes).

Writes assets/icon-1024.png, assets/icon-256.png and assets/icon.ico.
On macOS, also builds packaging/macos/edittr.icns with sips + iconutil.
Usage: python3 packaging/make_icon.py
"""
import math, os, struct, subprocess, sys, tempfile, zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "assets")


def png_bytes(w, h, rgba):
    raw = b"".join(b"\x00" + bytes(rgba[y * w * 4:(y + 1) * w * 4]) for y in range(h))
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - ax - t * dx, py - ay - t * dy)


def render(size):
    s = size / 1024.0
    # Shapes in 1024-space.
    trunk = (360, 250, 360, 774)                     # main branch line
    nodes_main = [(360, 250), (360, 774)]
    node_branch = (664, 400)
    # Branch curve: cubic from trunk (360,620) up to node_branch, sampled to segments.
    p0, p1, p2, p3 = (360, 640), (360, 500), (664, 560), node_branch
    curve = []
    for i in range(25):
        t = i / 24
        x = (1-t)**3*p0[0] + 3*(1-t)**2*t*p1[0] + 3*(1-t)*t*t*p2[0] + t**3*p3[0]
        y = (1-t)**3*p0[1] + 3*(1-t)**2*t*p1[1] + 3*(1-t)*t*t*p2[1] + t**3*p3[1]
        curve.append((x, y))
    stroke, r_node, r_ring = 34, 62, 30

    blue, green, white = (78, 154, 240), (120, 200, 120), (235, 240, 248)
    out = bytearray(size * size * 4)
    aa = 1.0 / s  # one output pixel, in 1024-space
    for y in range(size):
        for x in range(size):
            X, Y = (x + 0.5) / s, (y + 0.5) / s
            # Rounded-square background (macOS-style inset), with a vertical gradient.
            m, rad = 100, 185
            qx, qy = abs(X - 512) - (512 - m - rad), abs(Y - 512) - (512 - m - rad)
            bg_d = math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - rad
            a_bg = max(0.0, min(1.0, 0.5 - bg_d / aa))
            if a_bg <= 0:
                continue
            g = (Y - m) / (1024 - 2 * m)
            col = [36 + 10 * g, 41 + 12 * g, 54 + 20 * g]

            def over(color, d):
                a = max(0.0, min(1.0, 0.5 - d / aa))
                for i in range(3):
                    col[i] = col[i] * (1 - a) + color[i] * a

            over(blue, seg_dist(X, Y, *trunk) - stroke / 2)
            dc = min(seg_dist(X, Y, *curve[i], *curve[i + 1]) for i in range(len(curve) - 1))
            over(green, dc - stroke / 2)
            for n in nodes_main:
                over(blue, math.hypot(X - n[0], Y - n[1]) - r_node)
                over(white, math.hypot(X - n[0], Y - n[1]) - r_ring)
            over(green, math.hypot(X - node_branch[0], Y - node_branch[1]) - r_node)
            over(white, math.hypot(X - node_branch[0], Y - node_branch[1]) - r_ring)
            i = (y * size + x) * 4
            out[i:i + 4] = bytes([int(col[0]), int(col[1]), int(col[2]), int(255 * a_bg)])
    return out


def main():
    os.makedirs(ASSETS, exist_ok=True)
    pngs = {}
    for size in (16, 32, 48, 256, 1024):
        print(f"rendering {size}px", file=sys.stderr)
        pngs[size] = png_bytes(size, size, render(size))
    open(os.path.join(ASSETS, "icon-1024.png"), "wb").write(pngs[1024])
    open(os.path.join(ASSETS, "icon-256.png"), "wb").write(pngs[256])

    # ICO containing PNG-compressed entries (supported since Windows Vista).
    sizes = (16, 32, 48, 256)
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries, blobs = b"", b""
    for sz in sizes:
        data = pngs[sz]
        entries += struct.pack("<BBBBHHII", sz % 256, sz % 256, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    open(os.path.join(ASSETS, "icon.ico"), "wb").write(header + entries + blobs)

    if sys.platform == "darwin":
        with tempfile.TemporaryDirectory() as tmp:
            iconset = os.path.join(tmp, "edittr.iconset")
            os.makedirs(iconset)
            src = os.path.join(ASSETS, "icon-1024.png")
            for base in (16, 32, 128, 256, 512):
                for scale in (1, 2):
                    px = base * scale
                    name = f"icon_{base}x{base}{'@2x' if scale == 2 else ''}.png"
                    subprocess.run(["sips", "-z", str(px), str(px), src, "--out", os.path.join(iconset, name)],
                                   check=True, capture_output=True)
            dest = os.path.join(ROOT, "packaging", "macos", "edittr.icns")
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            subprocess.run(["iconutil", "-c", "icns", iconset, "-o", dest], check=True)


if __name__ == "__main__":
    main()
