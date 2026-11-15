"""o08 (case 1): per-slot look at the residual of QMF bands 0..3 (recovered slots of
reference L minus the model's) after a change registered while off, with the carry
model of o05 (lattice sections, state by band index, ducker by parameter-band index).

usage: o08_low_slots.py OUTDIR [policy k:v,...]"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')
pol = {'form': 'lat', 'carry': 'index', 'duck': 'index', 'zero': 'oldplain'}
if len(sys.argv) > 2:
    pol.update(dict(kv.split(':') for kv in sys.argv[2].split(',')))
for oc, nc in ((34, 20), (20, 34)):
    corner = 6
    frames = O1.stream('ro', oc, nc)
    p = os.path.join(src, 'o01_ro_%d_%d.aac' % (oc, nc))
    model = [f[1] for f in frames]
    y, msgs, _ = decode_log(p)
    X, _ = dump(p)
    comp = CarryComposer(pol)
    L, R = comp.run(X, model)
    d = y[:, 0] - psmodel.synthesis(L)[:len(y)]
    s0 = 32 * (corner - 1)
    Z, rr = recover(d[64 * s0:64 * (s0 + 160)], list(range(64)))
    print('== ro %d -> %d %s (recover %.1e)' % (oc, nc, pol, rr))
    Zy, _ = recover(y[64 * s0:64 * (s0 + 160), 0], list(range(64)))
    for k in range(2):
        print('   |ref L| k%d  %s' % (k, ' '.join('%.2f' % v for v in np.abs(Zy[32:64, k]) / 32768)))
        print('   |mod L| k%d  %s' % (k, ' '.join('%.2f' % v for v in np.abs(L[32 * corner:32 * corner + 32, k]) / 32768)))
        print('   |X|     k%d  %s' % (k, ' '.join('%.2f' % v for v in np.abs(X[32 * corner:32 * corner + 32, k]) / 32768)))
    for k in range(4):
        z = Z[32:160, k] / 32768
        print('   k%d' % k)
        for f in range(4):
            print('     f+%d %s' % (f, ' '.join('%.0e' % v for v in np.abs(z[32 * f:32 * f + 32]))))
