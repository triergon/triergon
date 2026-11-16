"""o02 (case 1): where the reference differs from the composed model (decorrelator reset)
on o01's streams: the residual of L (= D) recovered as QMF slots, per band and per slot,
in the first frame that runs after a change registered while off.

usage: o02_change_off_bands.py OUTDIR (o01's streams must be in OUTDIR/../o01)"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')
for old, new in ((34, 20), (20, 34)):
    for kind, corner in (('ro', 6), ('len', 5)):
        frames = O1.stream(kind, old, new)
        p = os.path.join(src, 'o01_%s_%d_%d.aac' % (kind, old, new))
        model = [f[1] for f in frames]
        y, msgs, _ = decode_log(p)
        X, _ = dump(p)
        for name, pol in (('reset', {}),):
            comp = Composer(pol)
            L, R = comp.run(X, model)
            d = y[:, 0] - psmodel.synthesis(L)[:len(y)]
            s0 = 32 * (corner - 1)
            seg = d[64 * s0:64 * (s0 + 96)]
            Z, rr = recover(seg, list(range(64)))
            Zc = Z[32:64]
            eb = np.sqrt(np.mean(np.abs(Zc) ** 2, 0))
            sig = np.sqrt(np.mean(np.abs(X[32 * corner:32 * corner + 32]) ** 2, 0)) / 32768
            print('== %s %d -> %d policy %s (recover rms %.1e)' % (kind, old, new, name, rr))
            print('   band rms of the residual / of X in the corner frame:')
            for k0 in range(0, 64, 8):
                print('   %2d: %s' % (k0, ' '.join('%.1e/%.0e' % (eb[k], sig[k]) for k in range(k0, k0 + 8))))
            print('   per-slot |residual| in bands 0..7 over the corner frame and the next:')
            for k in range(8):
                print('   k%d %s' % (k, ' '.join('%.0e' % v for v in np.abs(Z[32:96, k]))))
            for k in (8, 12, 20, 30, 40):
                print('   k%d %s' % (k, ' '.join('%.0e' % v for v in np.abs(Z[32:96, k]))))
