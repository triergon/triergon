#!/usr/bin/env python3
"""Recover the scaled-path chroma offsets (luma-domain integers) and grey LUT (research tooling).

Model under test (observed in observe_tables.py): R = L[Y + rv(V)], G = L[Y + gu(U) + gv(V)],
B = L[Y + bu(U)] with L the grey LUT clamped at its ends. For every matrix/range, constant blocks
sweep each chroma value against 64 luma levels; each offset must be the unique integer that
explains all 64 outputs. Saves OUT.npz with gray-CS-R and off-CS-R = int array [4, 256]
(rv, gu, gv, bu; gu/gv relative to gu(128)=gv(128)=0, verified additive on random triples).

usage: observe_offsets.py OUT.npz
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import run

B, G, OUT = 16, 32, 520
centre = np.array([int((i * B + B / 2) * OUT / 512) for i in range(G)])


def observe(combos):
    frames = []
    for f0 in range(0, len(combos), G * G):
        chunk = combos[f0:f0 + G * G]
        chunk = np.vstack([chunk, np.repeat(chunk[-1:], G * G - len(chunk), 0)])
        grid = chunk.reshape(G, G, 3)
        frames.append((np.kron(grid[..., 0], np.ones((B, B), int)), np.kron(grid[..., 1], np.ones((B // 2, B // 2), int)),
                       np.kron(grid[..., 2], np.ones((B // 2, B // 2), int))))
    return frames


def sample(out, n):
    return out[:, centre][:, :, centre][..., :3].reshape(-1, 3)[:n].astype(int)


res = {}
ys = np.arange(0, 256, 4)
rng_ = np.random.default_rng(9)
for cs in (1, 2, 4, 5, 6, 7, 9):  # 10 (bt2020c) is refused by the reference graph
    for r in (1, 2):
        frames = [(np.full((16, 16), v), np.full((8, 8), 128), np.full((8, 8), 128)) for v in range(256)]
        gray = run(frames, cs=cs, rng=r, chain='scale=32:24,format=rgba', out_shape=(24, 32))[:, 5, 5, 0].astype(int)
        L = lambda y: gray[np.clip(y, 0, 255)]
        c = np.arange(256)
        sweep_v = np.array([(y, 128, v) for v in c for y in ys])
        sweep_u = np.array([(y, u, 128) for u in c for y in ys])
        rand = rng_.integers(0, 256, (4096, 3))
        combos = np.vstack([sweep_v, sweep_u, rand])
        o = sample(run(observe(combos), cs=cs, rng=r, chain=f'scale={OUT}:{OUT},format=rgba', out_shape=(OUT, OUT)), len(combos))
        ov, ou, orand = o[:len(sweep_v)].reshape(256, 64, 3), o[len(sweep_v):2 * len(sweep_v)].reshape(256, 64, 3), o[2 * len(sweep_v):]
        def solve(obs):
            cand = [k for k in range(-400, 401) if (L(ys + k) == obs).all()]
            assert len(cand) >= 1, obs
            return cand
        table = np.zeros((4, 256), int); amb = 0
        for i in range(256):
            for row, obs in ((0, ov[i, :, 0]), (2, ov[i, :, 1]), (1, ou[i, :, 1]), (3, ou[i, :, 2])):
                cand = solve(obs); amb += len(cand) > 1; table[row, i] = cand[0]
        pred = np.stack([L(rand[:, 0] + table[0, rand[:, 2]]), L(rand[:, 0] + table[1, rand[:, 1]] + table[2, rand[:, 2]]),
                         L(rand[:, 0] + table[3, rand[:, 1]])], 1)
        res[f'gray-{cs}-{r}'] = gray; res[f'off-{cs}-{r}'] = table
        print(cs, r, 'ambiguous', amb, 'random-triple exact', float(np.mean(pred == orand)), flush=True)
np.savez_compressed(sys.argv[1], **res)
