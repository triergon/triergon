"""Per-frame max |diff| between candidate and reference interleaved f32le (dev aid)."""
import sys
import numpy as np
c = np.fromfile(sys.argv[1], dtype='<f4'); r = np.fromfile(sys.argv[2], dtype='<f4')
ch = int(sys.argv[3]); fl = int(sys.argv[4]) if len(sys.argv) > 4 else 1024
off = int(sys.argv[5]) if len(sys.argv) > 5 else 0  # candidate sample offset
c = c.reshape(-1, ch)[off:]; r = r.reshape(-1, ch)
n = min(len(c), len(r)); d = np.abs(c[:n] - r[:n])
print('cand', len(c), 'ref', len(r))
nf = n // fl
bad = [(f, d[f*fl:(f+1)*fl].max(axis=0)) for f in range(nf)]
cnt = 0
for f, m in bad:
    if m.max() > 6.1e-5:
        print('frame', f, 'max', ' '.join('%.2e' % x for x in m), 'refmax', '%.3f' % np.abs(r[f*fl:(f+1)*fl]).max())
        cnt += 1
        if cnt > 15: break
print('bad frames', sum(1 for f, m in bad if m.max() > 6.1e-5), 'of', nf)
