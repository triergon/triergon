# usage: delaymap.py <patched.ac3> <ref.f32> <out_channels> <au> <blk> <records_per_au> <coded->plane map e.g. 0:0,1:2,2:1> <au_bytes>
import sys, subprocess, numpy as np
from math import pi
from numpy import i0
src, reff, nout, au, blk, rpa, cmap, aub = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6]), sys.argv[7], int(sys.argv[8])
cmap = {int(a): int(b) for a, b in (x.split(':') for x in cmap.split(','))}
data = open(src, 'rb').read()
open('tmp/dm.in', 'wb').write(data[(au - 1) * aub:(au + 1) * aub])
import os
try: os.remove('tmp/dm.bin')
except FileNotFoundError: pass
subprocess.run(['./release/examples/decode', 'tmp/dm.in', '/dev/null'], env=dict(os.environ, AC3DUMP='tmp/dm.bin'), stderr=subprocess.DEVNULL)
d = np.fromfile('tmp/dm.bin', '<f4').reshape(-1, 7, 512).astype(np.float64)
B = rpa + blk  # record of AU 'au' (second AU in excerpt), block blk, first substream
ref = np.fromfile(reff, '<f4').reshape(-1, nout).astype(np.float64)
s = au * 1536 + blk * 256
M = 256; a = 5.0
k = np.array([i0(pi * a * np.sqrt(max(0, 1 - (2 * j / M - 1) ** 2))) for j in range(M + 1)])
w = np.sqrt(np.cumsum(k[:M]) / k.sum()); W = np.concatenate([w, w[::-1]])
def syn(x, N, alpha):
    n = np.arange(N)[:, None]; kk = np.arange(N // 2)[None, :]
    return (x[None, :] * np.cos(2 * pi / (4 * N) * (2 * n + 1) * (2 * kk + 1) + pi / 4 * (2 * kk + 1) * (1 + alpha))).sum(1)
def longfull(c): return -2 * W * syn(c, 512, 0.0)
def shortfull(c): return -2 * W * np.concatenate([syn(c[0::2], 256, -1.0), syn(c[1::2], 256, 1.0)])
for ch, pl in sorted(cmap.items()):
    t = ref[s:s + 256, pl]
    for nm, f in [('long', longfull), ('short', shortfull)]:
        imp = t - f(d[B, ch, :256])[:256]
        cands = {'zero': np.zeros(256)}
        for c in range(7):
            cands['delay%d' % c] = d[B, c, 256:]
        best = sorted(((np.abs(imp - v).max(), n) for n, v in cands.items()))[:2]
        print('coded', ch, 'plane', pl, nm, ['%s %.1e' % (n, e) for e, n in best], 'peak %.1e' % np.abs(t).max())
