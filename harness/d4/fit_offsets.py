#!/usr/bin/env python3
"""Fit the scaled-path chroma offset tables to 16.16 increments (research tooling).

Each observed table (observe_offsets.py) is off(i) = floor(i * inc / 65536) - floor(128 * inc / 65536)
for i in 0..255; entries the observation cannot pin (clipped for every sampled luma) are skipped.
Every inc in the printed range yields the identical table on 0..255. The grey LUT is fitted as
clip(floor((Y * cy + oy) / 65536)). usage: fit_offsets.py OFFSETS.npz
"""
import sys, json
import numpy as np
d = np.load(sys.argv[1])
i = np.arange(256); ys = np.arange(0, 256, 4)
out = {}
for key in sorted(k[4:] for k in d.files if k.startswith('off-')):
    t = d['off-' + key]; g = d['gray-' + key]
    L = lambda y: g[np.clip(y, 0, 255)]
    pinned = np.array([[not ((L(ys + t[r, j]) == L(ys + t[r, j] + 1)).all() or (L(ys + t[r, j]) == L(ys + t[r, j] - 1)).all())
                        for j in range(256)] for r in range(4)])
    incs = []
    for r in range(4):
        k0 = int(round((t[r, 200] - t[r, 56]) / 144 * 65536))
        c = [inc for inc in range(k0 - 3000, k0 + 3000)
             if (((i * inc) // 65536 - (128 * inc) // 65536)[pinned[r]] == t[r][pinned[r]]).all()]
        incs.append([c[0], c[-1]])
    # grey LUT
    lut = None
    for cy in range(65536 if key.endswith('-2') else 76000, (65537 if key.endswith('-2') else 76700)):
        lo, hi = -10**9, 10**9
        for y, v in zip(i, g):
            if 0 < v < 255: lo, hi = max(lo, v * 65536 - y * cy), min(hi, (v + 1) * 65536 - y * cy - 1)
            elif v == 0: hi = min(hi, 65536 - y * cy - 1)
            else: lo = max(lo, 255 * 65536 - y * cy)
        if lo <= hi and (lut is None or abs(cy - 76309) < abs(lut[0] - 76309)): lut = [cy, int(lo), int(hi)]
    out[key] = {'rv_gu_gv_bu_inc_ranges': incs, 'grey_cy_oy_range': lut, 'unpinned_entries': int((~pinned).sum())}
    print(key, incs, lut, int((~pinned).sum()))
json.dump(out, open(sys.argv[1].replace('.npz', '-fit.json'), 'w'), indent=1)
