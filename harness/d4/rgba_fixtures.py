#!/usr/bin/env python3
"""Capture reference digests of letterboxed synthetic pictures (research tooling).

Each case is a pseudo-random 4:2:0 picture (the LCG below, mirrored in crates/sim/tests/rgba_reference.rs)
converted by the reference chain (harness/d4/probe_rgba.c, full chain including pad). The digest is the
M1 fold h = h * 31 + byte (mod 2^64) over the whole 1920x1080 RGBA canvas.
usage: rgba_fixtures.py OUT.txt
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import run

CASES = [  # width height bits matrix range(1 tv, 2 pc) seed
    (1280, 720, 8, 1, 1, 1), (352, 288, 8, 5, 1, 2), (416, 240, 8, 5, 2, 3), (720, 480, 8, 6, 1, 4),
    (2560, 1600, 8, 4, 1, 5), (300, 168, 8, 7, 2, 6), (1920, 1080, 8, 1, 1, 7), (1920, 800, 8, 1, 2, 8),
    (3840, 2160, 10, 9, 1, 9), (1920, 1080, 10, 9, 1, 10), (64, 240, 10, 1, 2, 11), (1280, 720, 10, 2, 1, 12),
    # odd fitted widths (1895, 961, 135): the full-width chroma output stage
    (414, 236, 8, 5, 1, 13), (210, 236, 10, 1, 2, 14), (528, 4216, 8, 7, 1, 15),
]


def lcg_plane(state, n, bits):
    out = np.empty(n, dtype=np.uint64)
    a, c, mask = 6364136223846793005, 1442695040888963407, (1 << 64) - 1
    s = state
    for i in range(n):
        s = (s * a + c) & mask
        out[i] = (s >> 33) & ((1 << bits) - 1)
    return s, out


def picture(w, h, bits, seed):
    s = seed
    s, y = lcg_plane(s, w * h, bits)
    cw, ch = (w + 1) // 2, (h + 1) // 2
    s, u = lcg_plane(s, cw * ch, bits)
    s, v = lcg_plane(s, cw * ch, bits)
    return y.reshape(h, w).astype(np.int64), u.reshape(ch, cw).astype(np.int64), v.reshape(ch, cw).astype(np.int64)


def digest(canvas):
    b = canvas.reshape(-1).astype(np.uint64)
    pw = np.empty(len(b), dtype=np.uint64); pw[0] = 1
    with np.errstate(over='ignore'):
        pw[1:] = np.uint64(31)
        pw = np.cumprod(pw, dtype=np.uint64)[::-1]
        return int((b * pw).sum(dtype=np.uint64))


if __name__ == '__main__':
    lines = ['# width height bits matrix full_range seed digest  (harness/d4/rgba_fixtures.py, reference FFmpeg 9.0.2 dev Mac)']
    for w, h, bits, cs, r, seed in CASES:
        canvas = run([picture(w, h, bits, seed)], bits=bits, cs=cs, rng=r)[0]
        lines.append(f'{w} {h} {bits} {cs} {int(r == 2)} {seed} {digest(canvas)}')
        print(lines[-1], flush=True)
    open(sys.argv[1], 'w').write('\n'.join(lines) + '\n')
