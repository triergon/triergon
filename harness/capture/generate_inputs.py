#!/usr/bin/env python3
"""Deterministic RGBA capture inputs for the D5/O62 required cases.

All content is authored here from seeded arithmetic: no third-party media and
therefore no redistribution question. Each case regenerates byte-identically
and the qualification run records the SHA-256 of every input it used.

Content classes (frames are tightly packed RGBA, 4 bytes per pixel):
  ui_static      one synthetic UI screen repeated (keepalive / idle screen)
  ui_scroll      a tall UI canvas scrolled 6 px per frame with a moving focus ring
  ui_transition  crossfade between two UI screens, then hold
  motion         panning smooth texture with three moving textured objects
  noise          independent uniform noise per frame (encoder worst case)
Alpha is varied pseudo-randomly in ui_static to exercise "alpha ignored".
"""
import argparse
import hashlib
import sys

import numpy as np


def ui_screen(w, h, seed, height=None):
    """A Streem-like UI: gradient backdrop, header, poster cards, text rows."""
    height = height or h
    rng = np.random.default_rng(seed)
    img = np.zeros((height, w, 3), np.float32)
    y = np.linspace(0, 1, height)[:, None]
    img[..., 0] = 18 + 30 * y
    img[..., 1] = 20 + 22 * y
    img[..., 2] = 38 + 60 * y
    s = w / 960.0
    img[: int(56 * s)] = (28, 30, 44)

    def text_row(x0, y0, width, size, color):
        x = x0
        while x < x0 + width:
            glyph = int(rng.integers(3, 9) * s) + 1
            if rng.random() < 0.15:
                x += glyph
                continue
            for gy in range(size):
                mask = rng.random(glyph) < 0.55
                row = y0 + gy
                if 0 <= row < height:
                    seg = img[row, x:x + glyph]
                    seg[mask[: seg.shape[0]]] = color
            x += glyph + max(1, int(s))

    text_row(int(24 * s), int(18 * s), int(300 * s), max(2, int(14 * s)), (230, 230, 235))
    card_w, card_h, gap = int(150 * s), int(220 * s), int(24 * s)
    top = int(90 * s)
    while top + card_h < height:
        left = int(40 * s)
        while left + card_w < w:
            c0 = rng.integers(40, 220, 3).astype(np.float32)
            c1 = rng.integers(10, 120, 3).astype(np.float32)
            t = np.linspace(0, 1, card_h)[:, None, None]
            img[top:top + card_h, left:left + card_w] = c0 * (1 - t) + c1 * t
            img[top:top + card_h, left:left + 1] = 200
            img[top:top + card_h, left + card_w - 1:left + card_w] = 200
            text_row(left, top + card_h + int(6 * s), card_w, max(2, int(10 * s)), (190, 190, 200))
            left += card_w + gap
        top += card_h + int(48 * s)
    return np.clip(img, 0, 255).astype(np.uint8)


def rgba(rgb, alpha=None):
    a = np.full(rgb.shape[:2] + (1,), 255, np.uint8) if alpha is None else alpha[..., None]
    return np.concatenate([rgb, a], axis=2)


def frames_ui_static(w, h, n):
    rgb = ui_screen(w, h, 1)
    alpha = np.random.default_rng(2).integers(0, 256, (h, w), dtype=np.uint8)
    one = rgba(rgb, alpha).tobytes()
    for _ in range(n):
        yield one


def frames_ui_scroll(w, h, n):
    canvas = ui_screen(w, h, 3, height=h + 6 * n + 16)
    s = w / 960.0
    for k in range(n):
        view = canvas[6 * k: 6 * k + h].copy()
        # Focus ring moves to the next card every 10 frames.
        fx = int((40 + (k // 10 % 5) * 174) * s)
        fy = int(90 * s)
        ring = (255, 200, 40)
        view[fy:fy + 3, fx:fx + int(150 * s)] = ring
        view[fy + int(220 * s) - 3:fy + int(220 * s), fx:fx + int(150 * s)] = ring
        yield rgba(view).tobytes()


def frames_ui_transition(w, h, n):
    a = ui_screen(w, h, 4).astype(np.float32)
    b = ui_screen(w, h, 5).astype(np.float32)
    for k in range(n):
        t = min(max((k - 10) / 30.0, 0.0), 1.0)
        yield rgba(np.clip(a * (1 - t) + b * t + 0.5, 0, 255).astype(np.uint8)).tobytes()


def frames_motion(w, h, n):
    rng = np.random.default_rng(6)
    yy, xx = np.mgrid[0:h, 0:w].astype(np.float32)
    grain = rng.normal(0, 3, (h, w, 3)).astype(np.float32)
    objects = [(rng.uniform(0, w), rng.uniform(0, h), rng.uniform(-9, 9), rng.uniform(-6, 6), rng.uniform(30, 90) * w / 960)
               for _ in range(3)]
    for k in range(n):
        px, py = 3.0 * k, 1.5 * k
        r = 120 + 70 * np.sin((xx + px) / 37.0) + 30 * np.cos((yy + py) / 23.0)
        g = 110 + 60 * np.cos((xx + px) / 51.0 + (yy + py) / 41.0)
        b = 130 + 50 * np.sin((yy + py) / 29.0) + 25 * np.sin((xx + px) / 13.0)
        img = np.stack([r, g, b], axis=2) + grain
        for ox, oy, vx, vy, rad in objects:
            cx, cy = (ox + vx * k) % w, (oy + vy * k) % h
            d = (xx - cx) ** 2 + (yy - cy) ** 2
            inside = d < rad * rad
            stripes = 128 + 100 * np.sin((xx - cx) / 4.0 + (yy - cy) / 7.0)
            img[inside] = np.stack([stripes, 255 - stripes, stripes * 0.5 + 60], axis=2)[inside]
        yield rgba(np.clip(img, 0, 255).astype(np.uint8)).tobytes()


def frames_noise(w, h, n):
    rng = np.random.default_rng(7)
    for _ in range(n):
        yield rgba(rng.integers(0, 256, (h, w, 3), dtype=np.uint8)).tobytes()


CONTENT = {
    "ui_static": frames_ui_static,
    "ui_scroll": frames_ui_scroll,
    "ui_transition": frames_ui_transition,
    "motion": frames_motion,
    "noise": frames_noise,
}


def generate(content, w, h, n, path):
    digest = hashlib.sha256()
    with open(path, "wb") as f:
        for frame in CONTENT[content](w, h, n):
            assert len(frame) == w * h * 4
            f.write(frame)
            digest.update(frame)
    return digest.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("content", choices=sorted(CONTENT))
    ap.add_argument("width", type=int)
    ap.add_argument("height", type=int)
    ap.add_argument("frames", type=int)
    ap.add_argument("out")
    a = ap.parse_args()
    print(generate(a.content, a.width, a.height, a.frames, a.out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
