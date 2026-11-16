"""o11 (case 1): reference L against the model in QMF bands 0 and 1, slot by slot, in
o09's silent corner frame (the output is the carried decorrelator state alone): the
recovered reference slots, the model's, and their difference.

usage: o11_qmf0_slots.py OUTDIR [seed]"""
import o09_silent_corner as O9

from olib import *

out = sys.argv[1]
seed = int(sys.argv[2]) if len(sys.argv) > 2 else 21
for oc, nc in ((34, 20), (20, 34)):
    p, model, y, X, comp, L = O9.run(oc, nc, seed, show=False)
    corner = 6
    s0 = 32 * (corner - 1)
    Zy, rr = recover(y[64 * s0:64 * (s0 + 128), 0], list(range(64)))
    print('== ro %d -> %d seed %d (recover %.1e)' % (oc, nc, seed, rr))
    for k in (0, 1, 2):
        ref = Zy[32:96, k] / 32768
        mod = L[32 * corner:32 * corner + 64, k] / 32768
        print('   QMF %d   t: |ref|   |model|   |ref - model|   ref/model' % k)
        for t in list(range(0, 12)) + list(range(28, 36)):
            q = ref[t] / mod[t] if abs(mod[t]) > 0 else 0
            print('     %2d  %.2e  %.2e  %.2e  %.3f@%+.1f' % (t, abs(ref[t]), abs(mod[t]), abs(ref[t] - mod[t]),
                                                         abs(q), np.degrees(np.angle(q))))
