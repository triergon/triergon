# Noise model check for AHT zero bins and SPX. usage: noise.py <stream> <ref.f32> <nch> <planes>
import sys, os, subprocess, json, numpy as np
from math import pi
from numpy import i0
stream, reff, nch = sys.argv[1], sys.argv[2], int(sys.argv[3])
planes = [int(x) for x in sys.argv[4].split(',')]
if os.path.exists('tmp/nz.dump'): os.remove('tmp/nz.dump')
r = subprocess.run(['./dbg/release/examples/decode', stream, '/dev/null'], env=dict(os.environ, AC3DUMP='tmp/nz.dump', AC3J='1'), stderr=subprocess.PIPE, text=True)
J = [json.loads(l[2:]) for l in r.stderr.splitlines() if l.startswith('J ')]
d = np.fromfile('tmp/nz.dump', '<f4').reshape(-1, 7, 512).astype(np.float64)
ref = np.fromfile(reff, '<f4').reshape(-1, nch).astype(np.float64)
M = 256; a = 5.0
k = np.array([i0(pi * a * np.sqrt(max(0, 1 - (2 * j / M - 1) ** 2))) for j in range(M + 1)])
w = np.sqrt(np.cumsum(k[:M]) / k.sum()); W = np.concatenate([w, w[::-1]])
n = np.arange(512)[None, :]; kk = np.arange(256)[:, None]
F = np.cos(2 * pi / (4 * 512) * (2 * n + 1) * (2 * kk + 1) + pi / 4 * (2 * kk + 1)) * W[None, :] / -256.0
nb = min(len(ref) // 256 - 1, len(J))
X = {c: np.stack([F @ ref[256 * b:256 * b + 512, planes[c]] for b in range(nb)]) for c in range(len(planes)) if planes[c] >= 0}
REM = [13, 25, 37, 61, 253]
zero_vals = []; spx_resid = []; spx_out = []; spx_pred_err = []
def atten(code, b): return 2 ** (-(code + 1) * (b + 1) / 15)
for b in range(1, nb):
    j = J[b]
    for c in X:
        ff = X[c][b].copy()
        g = j['dyn'][0]
        # AHT zero bins, not rematrixed
        if j['aht'][c] and c < 2:
            for i in range(j['end'][c]):
                if j['bap'][c][i] != 0: continue
                remat = False
                if j['nremat'] and c < 2 and i >= 13:
                    bnd = max(x for x in range(4) if REM[x] <= i)
                    remat = bnd < j['nremat'] and j['remat'][bnd]
                v = ff[i]
                if remat:
                    v = (X[0][b][i] + X[1][b][i]) / 2 if c == 0 else (X[0][b][i] - X[1][b][i]) / 2
                zero_vals.append(v / g * 2 ** j['exps'][c][i])
        if c < 5 and j['chinspx'][c] and j['spx'][1] > 0:
            # predict from FFmpeg's own baseband
            tc = ff.copy()
            copystart = j['spxstrtf']; copyend = j['spx'][0]
            sizes = j['spxbndsz']
            cp = copystart; ins = copyend; wrap = []
            for bs in sizes:
                if cp + bs > copyend: cp = copystart; wrap.append(True)
                else: wrap.append(False)
                for _ in range(bs):
                    if cp == copyend: cp = copystart
                    tc[ins] = tc[cp]; ins += 1; cp += 1
            m = copyend; rms = []
            for bs in sizes:
                rms.append(np.sqrt(np.mean(tc[m:m + bs] ** 2))); m += bs
            if j['atten'][c]:
                code = j['attencod'][c]; at = [atten(code, 0), atten(code, 1), atten(code, 2)]
                def notch(bd):
                    tc[bd - 2] *= at[0]; tc[bd - 1] *= at[1]; tc[bd] *= at[2]; tc[bd + 1] *= at[1]; tc[bd + 2] *= at[0]
                notch(copyend); bd = copyend + sizes[0]
                for q in range(1, len(sizes)):
                    if wrap[q]: notch(bd)
                    bd += sizes[q]
            m = copyend
            for q, bs in enumerate(sizes):
                co = j['spxco'][c][q] * 32
                pred = tc[m:m + bs] * j['sblend'][c][q] * co
                nsc = rms[q] * j['nblend'][c][q] * co
                res = ff[m:m + bs] - pred
                if nsc > 0:
                    rr = res / nsc
                    spx_resid.extend(rr)
                    for o in np.where(np.abs(rr) > 1.75)[0]: spx_out.append((b, c, m + o, q, rr[o]))
                m += bs
zv = np.array(zero_vals)
print('AHT zero-bin values *2^exp/gain: n=%d mean %.3f std %.3f min %.3f max %.3f' % (len(zv), zv.mean(), zv.std(), zv.min(), zv.max()) if len(zv) else 'no AHT zero bins')
if len(zv): print('   quantiles', np.round(np.percentile(zv, [0, 1, 5, 25, 50, 75, 95, 99, 100]), 3))
sr = np.array(spx_resid)
if len(sr):
    print('SPX normalised noise residual: n=%d mean %.4f std %.4f min %.3f max %.3f' % (len(sr), sr.mean(), sr.std(), sr.min(), sr.max()))
    print('   quantiles', np.round(np.percentile(sr, [0, 1, 5, 25, 50, 75, 95, 99, 100]), 3))

from collections import Counter
print('outliers', len(spx_out), Counter((o[0], o[1]) for o in spx_out).most_common(8))
print(spx_out[:12])
