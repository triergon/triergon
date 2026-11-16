"""o03 (case 1): the ducking gain of the reference in the first frame that runs after a
change registered while off, read in the QMF bands with a 1-slot plain delay (QMF >= 35:
D[t] = g[t] x[t - 1], so with ICC 7 L / x[t - 1] is the gain from slot 1 on). The gains
are compared with the ducker of the composed model started from zero and from the old
layout's state carried by parameter-band index.

usage: o03_duck_readout.py OUTDIR (o01's streams in OUTDIR/../o01)"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')
QK = list(range(37, 61))   # bands well inside the 1-slot region (both layouts)
for old, new in ((34, 20), (20, 34)):
    for kind, corner in (('ro', 6), ('len', 5), ('err', 5), ('direct', 4)):
        frames = O1.stream(kind, old, new)
        p = os.path.join(src, 'o01_%s_%d_%d.aac' % (kind, old, new))
        model = [f[1] for f in frames]
        y, msgs, _ = decode_log(p)
        X, _ = dump(p)
        s0 = 32 * (corner - 1)
        Z, rr = recover(y[64 * s0:64 * (s0 + 96), 0], list(range(64)))
        Zc = Z[32:64]                       # reference L slots of the corner frame
        xprev = X[32 * corner - 1:32 * corner + 31]
        gref = np.array([np.vdot(xprev[t, QK], Zc[t, QK]).real / np.vdot(xprev[t, QK], xprev[t, QK]).real
                         for t in range(32)])
        print('== %s %d -> %d (recover rms %.1e)' % (kind, old, new, rr))
        print('   ref    ', ' '.join('%.4f' % v for v in gref[:20]))
        for name, pol in (('reset', {}), ('duck-index', {'duck': 'index'}), ('index,duck-index', {'dec': 'index', 'duck': 'index'})):
            comp = Composer(pol)
            L, R = comp.run(X, model)
            Lc = L[32 * corner:32 * corner + 32]
            g = np.array([np.vdot(xprev[t, QK], Lc[t, QK]).real / np.vdot(xprev[t, QK], xprev[t, QK]).real
                          for t in range(32)])
            print('   %-7s' % name[:7], ' '.join('%.4f' % v for v in g[:20]), ' max dev %.1e' % np.abs(g - gref)[1:].max())
