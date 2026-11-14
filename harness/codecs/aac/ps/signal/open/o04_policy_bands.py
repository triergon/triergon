"""o04 (case 1): per-QMF-band look (analysis of the residual) at the corner frame of o01's
ro and len streams under a few carry-over policies.

usage: o04_policy_bands.py OUTDIR [policy=k:v,... ...]"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')
pols = [dict(kv.split(':') for kv in a.split(',')) if a != '-' else {} for a in sys.argv[2:]] or [{}]
for old, new in ((34, 20), (20, 34)):
    for kind, corner in (('ro', 6), ('len', 5)):
        frames = O1.stream(kind, old, new)
        p = os.path.join(src, 'o01_%s_%d_%d.aac' % (kind, old, new))
        model = [f[1] for f in frames]
        y, msgs, _ = decode_log(p)
        X, _ = dump(p)
        for pol in pols:
            comp = Composer(pol)
            L, R = comp.run(X, model)
            d = y[:, 0] - psmodel.synthesis(L)[:len(y)]
            r = per_frame(model_out(L, R, len(y)), y).max(1)
            Dq = psana.qmf_bands(np.concatenate([np.zeros(640), d]))
            s = slice(32 * corner + 10, 32 * corner + 42)
            eb = np.sqrt((np.abs(Dq[s]) ** 2).mean(0)) / 32768
            print('== %s %d -> %d %s: frames %s' % (kind, old, new, pol, fmt(r[corner - 1:corner + 4])))
            for k0 in range(0, 64, 16):
                print('   %2d: %s' % (k0, fmt(eb[k0:k0 + 16])))
