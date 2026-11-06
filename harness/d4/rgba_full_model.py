#!/usr/bin/env python3
"""numpy model of the complete reference RGBA path, checked against the probe (research tooling).

Uses rgba_model.taps for every 1-D pass and the fitted chroma/grey tables (TABLES below, from
fit_offsets.py). usage: rgba_full_model.py [WxH[:bits[:cs[:range]]] ...]
"""
import sys, os, functools; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import run, fit
from rgba_model import taps

# (matrix group) -> (range 1=tv, 2=pc) -> [rv, gu, gv, bu] 16.16 increments (middle of the fitted ranges)
TABLES = {
    '601': {1: [89831, -22048, -45755, 113537], 2: [91881, -22552, -46793, 116128]},
    '709': {1: [100902, -12002, -29994, 118894], 2: [103205, -12280, -30680, 121608]},
    'fcc': {1: [89702, -21264, -45612, 114052], 2: [91725, -21748, -46650, 116651]},
    '240m': {1: [100981, -14519, -30540, 116998], 2: [103286, -14850, -31236, 119668]},
    '2020': {1: [94483, -10545, -36608, 120546], 2: [96640, -10783, -37431, 123299]},
}
GROUP = {1: '709', 4: 'fcc', 7: '240m', 9: '2020'}
GREY_TV = (76309, -1304880)


def tables(cs, rng):
    inc = TABLES[GROUP.get(cs, '601')][rng]
    i = np.arange(256)
    off = np.array([(i * k) // 65536 - (128 * k) // 65536 for k in inc])
    y = np.arange(256)
    grey = y if rng == 2 else np.clip((y * GREY_TV[0] + GREY_TV[1]) // 65536, 0, 255)
    return off, grey


@functools.lru_cache(None)
def mat(n, m, one):
    M = np.zeros((m, n), dtype=np.int64)
    for x, (s, c) in enumerate(taps(n, m, one)):
        M[x, s:s + len(c)] = c
    return M


def model(Y, U, V, cs, rng, bits):
    h, w = Y.shape; fw, fh, _, _ = fit(w, h); sh = bits - 1
    def plane(P, ow, oh):
        H = np.minimum((P.astype(np.int64) @ mat(P.shape[1], ow, 16384).T) >> sh, 32767)
        return (mat(P.shape[0], oh, 4096) @ H + (1 << 18)) >> 19
    cw = (fw + 1) // 2
    y = plane(Y, fw, fh)
    u = np.repeat(np.clip(plane(U, cw, fh), 0, 255), 2, 1)[:, :fw]
    v = np.repeat(np.clip(plane(V, cw, fh), 0, 255), 2, 1)[:, :fw]
    off, grey = tables(cs, rng)
    L = lambda i: grey[np.clip(i, 0, 255)]
    return np.stack([L(y + off[0][v]), L(y + off[1][u] + off[2][v]), L(y + off[3][u])], -1)


if __name__ == '__main__':
    gen = np.random.default_rng(21)
    for spec in sys.argv[1:]:
        parts = spec.split(':'); w, h = map(int, parts[0].split('x'))
        bits = int(parts[1]) if len(parts) > 1 else 8
        cs = int(parts[2]) if len(parts) > 2 else 1; r = int(parts[3]) if len(parts) > 3 else 1
        top = 1 << bits
        Y = gen.integers(0, top, (h, w)); U = gen.integers(0, top, ((h + 1) // 2, (w + 1) // 2)); V = gen.integers(0, top, ((h + 1) // 2, (w + 1) // 2))
        fw, fh, _, _ = fit(w, h)
        if (fw, fh) == (w, h) and bits == 8:
            print(spec, 'identity 8-bit uses the unscaled converter; skipped'); continue
        ref = run([(Y, U, V)], bits=bits, cs=cs, rng=r, chain='scale=1920:1080:force_original_aspect_ratio=decrease,format=rgba', out_shape=(fh, fw))[0][..., :3].astype(int)
        d = model(Y, U, V, cs, r, bits) != ref
        bad = d.any(-1)
        print(spec, f'{fw}x{fh}', 'pixels exact', f'{1 - bad.mean():.7f}', 'bad', int(bad.sum()),
              'bad cols', sorted(set(np.nonzero(bad)[1].tolist()))[:6], 'bad rows', sorted(set(np.nonzero(bad)[0].tolist()))[:6], flush=True)
