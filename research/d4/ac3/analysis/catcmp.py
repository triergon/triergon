# Categorised MDCT-domain comparison. usage: catcmp.py <stream> <ref.f32> <nch> <planes csv coded->plane> [substream_records_per_block=1]
import sys, os, subprocess, json, numpy as np
from math import pi
from numpy import i0
stream, reff, nch = sys.argv[1], sys.argv[2], int(sys.argv[3])
planes = [int(x) for x in sys.argv[4].split(',')]
for f in ['tmp/cc.dump']:
    if os.path.exists(f): os.remove(f)
r = subprocess.run(['./dbg/release/examples/decode', stream, '/dev/null'], env=dict(os.environ, AC3DUMP='tmp/cc.dump', AC3J='1'), stderr=subprocess.PIPE, text=True)
J = [json.loads(l[2:]) for l in r.stderr.splitlines() if l.startswith('J ')]
d = np.fromfile('tmp/cc.dump', '<f4').reshape(-1, 7, 512).astype(np.float64)
ref = np.fromfile(reff, '<f4').reshape(-1, nch).astype(np.float64)
M = 256; a = 5.0
k = np.array([i0(pi * a * np.sqrt(max(0, 1 - (2 * j / M - 1) ** 2))) for j in range(M + 1)])
w = np.sqrt(np.cumsum(k[:M]) / k.sum()); W = np.concatenate([w, w[::-1]])
n = np.arange(512)[None, :]; kk = np.arange(256)[:, None]
F = np.cos(2 * pi / (4 * 512) * (2 * n + 1) * (2 * kk + 1) + pi / 4 * (2 * kk + 1)) * W[None, :] / -256.0
nb = min(len(ref) // 256 - 1, len(J))
X = {c: np.stack([F @ ref[256 * b:256 * b + 512, planes[c]] for b in range(nb)]) for c in range(len(planes)) if planes[c] >= 0}
REM = [13, 25, 37, 61, 253]
cats = {}
def add(cat, err, mag):
    cats.setdefault(cat, []).append((err, mag))
for b in range(nb):
    j = J[b]
    bap = j['bap']
    for c in X:
        mine = d[b, c, :256]; ff = X[c][b]
        spx0 = j['spx'][0] if c < 5 and j['chinspx'][c] else 999
        for i in range(256):
            e = abs(mine[i] - ff[i]); m = abs(mine[i])
            if i >= spx0 and i < j['spx'][1]:
                add('spx', e, m); continue
            # coded channels contributing
            srcs = [c]
            if j['nremat'] and c < 2 and i >= 13:
                bnd = max(x for x in range(4) if REM[x] <= i)
                if bnd < j['nremat'] and j['remat'][bnd] and i < min(j['end'][0], j['end'][1]):
                    srcs = [0, 1]
            cplsrc = j['cpl'] and j['cplrange'][0] <= i < j['cplrange'][1]
            if cplsrc: srcs = srcs + [5]
            if i >= max(j['end'][s] for s in srcs if s != 5) and not cplsrc:
                add('above_end', e, m); continue
            zero = any(bap[s][i] == 0 for s in srcs)
            add('bap0' if zero else 'det', e, m)
for cat, v in cats.items():
    e = np.array([x[0] for x in v]); m = np.array([x[1] for x in v])
    rel = e / np.maximum(m, 1e-12)
    print('%-10s n=%7d  max abs %.2e  p99 abs %.2e  median rel %.1e  frac(abs>1e-7) %.4f' % (cat, len(e), e.max(), np.percentile(e, 99), np.median(rel), np.mean(e > 1e-7)))
