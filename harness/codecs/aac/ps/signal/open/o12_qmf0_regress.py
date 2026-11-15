"""o12 (case 1): the state the QMF-0 bands of the new layout start from after a change
registered while off, as a linear combination of the old layout's band states, fitted
jointly over several seeds of o09's silent-corner streams. Basis: for each new QMF-0 band
j and each old all-pass band i, the response of band j started from old band i's whole
state (as stored, and conjugated), times the ducking gain; one complex coefficient per
(j, i, conj), shared by all seeds. Every other band is carried as in o05.

usage: o12_qmf0_regress.py OUTDIR [seeds]"""
import o10_silent_search as O10

from olib import *

out = sys.argv[1]
SEEDS = [int(s) for s in sys.argv[2].split(',')] if len(sys.argv) > 2 else list(range(31, 41))
QK = (0,)


def regress(oc, nc):
    rows, ys = [], []
    for seed in SEEDS:
        r, D, low, keys, tmpl, old = O10.setup(oc, nc, seed)
        # the residual is relative to the index carry; add the index-carry term back
        js = [j for j in low if tmpl.bands[j][0] in QK]
        rr = r.copy()
        for j in js:
            rr += D[(j, (j, False))]
        keys = [k for k in keys if k[0] is not None]
        cols = []
        names = []
        for j in js:
            for key in keys:
                v = D[(j, key)] - D[(j, (None, False))]     # response of the state alone
                cols += [v]
                names.append((j, key))
        rows.append(np.array(cols).T)
        ys.append(rr - sum(D[(j, (None, False))] for j in js))
    A = np.concatenate(rows)
    b = np.concatenate(ys)
    a, *_ = np.linalg.lstsq(A, b, rcond=None)
    res = b - A @ a
    print('== ro %d -> %d, %d seeds: rms %.2e -> %.2e, %d coefficients' % (
        oc, nc, len(SEEDS), np.sqrt(np.mean(b ** 2)), np.sqrt(np.mean(res ** 2)), len(a)))
    for j in js:
        k, bb, f, kd = tmpl.bands[j]
        big = sorted([(abs(c), key, c) for (jj, key), c in zip(names, a) if jj == j], key=lambda t: -t[0])[:4]
        print('   new %2d (QMF %d f %+.3f par %d): %s' % (j, k, f, bb, ', '.join(
            '%.3f x old %d%s (f %+.3f)' % (c, key[0], '*' if key[1] else '', old.bands[key[0]][2]) for m, key, c in big)))


if __name__ == '__main__':
    for oc, nc in ((34, 20), (20, 34)):
        regress(oc, nc)
