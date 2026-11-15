"""o10 (case 1): on o09's silent-corner streams (several seeds), which old band's state
(as stored, or complex conjugated, or none) each low hybrid band of QMF 0 and 1 in the new
layout starts from: a coordinate descent over the time-domain residual of L summed over
the seeds, every other band carried as in o05.

usage: o10_silent_search.py OUTDIR [seeds]"""
import o09_silent_corner as O9

from olib import *

out = sys.argv[1]
SEEDS = [int(s) for s in sys.argv[2].split(',')] if len(sys.argv) > 2 else [21, 22, 23]
NF = 3
FORM = 'lat'


def candidates(old, new_cfg, Hc):
    """{(old band or None, conj): D of every new band started from that state}."""
    out = {}
    keys = [(None, False)] + [(i, c) for i in range(len(old.bands)) if old.kind[i] == 0 for c in (False, True)]
    for i, conj in keys:
        dec = FormDecorrelator(new_cfg, FORM)
        if i is not None:
            cj = np.conj if conj else (lambda v: v)
            dec.hin[:] = cj(old.hin[:, i])[:, None]
            for m in range(3):
                dec.w[m][:] = cj(old.w[m][:, i])[:, None]
        n0 = min(old.npar, dec.npar)
        dec.pk[:n0], dec.e1[:n0], dec.e2[:n0] = old.pk[:n0], old.e1[:n0], old.e2[:n0]
        out[(i, conj)] = dec.process(Hc)
    return out


def setup(old_cfg, new_cfg, seed):
    p, model, y, X, comp, L = O9.run(old_cfg, new_cfg, seed, show=False)
    corner = 6
    old = comp.old_dec
    runs = [f for f in range(corner, len(comp.outs)) if comp.outs[f].processed][:NF]
    Hc = np.concatenate([comp.trace[f][0] for f in runs])
    Dbase = np.concatenate([comp.trace[f][1] for f in runs])
    s0 = 32 * corner
    lo, hi = 64 * s0, 64 * (s0 + 32 * NF)
    r = (y[:, 0] - psmodel.synthesis(L)[:len(y)])[lo:hi]
    tmpl = FormDecorrelator(new_cfg, FORM)
    low = [j for j, b in enumerate(tmpl.bands) if b[3] == 'low' and b[0] < 2]
    cand = candidates(old, new_cfg, Hc)
    n = len(Hc)
    D = {}
    for j in low:
        k = tmpl.bands[j][0]
        for key, Dk in cand.items():
            full = np.zeros(s0 + n, complex)
            full[s0:] = Dk[:, j] - Dbase[:, j]
            D[(j, key)] = psmodel.synth_band(full, k)[lo:hi]
    return r, D, low, list(cand), tmpl, old


def search(old_cfg, new_cfg):
    sets = [setup(old_cfg, new_cfg, s) for s in SEEDS]
    low, keys, tmpl, old = sets[0][2], sets[0][3], sets[0][4], sets[0][5]
    choice = {j: (j, False) for j in low}
    resid = [s[0].copy() for s in sets]
    for j in low:
        for r, s in zip(resid, sets):
            r -= s[1][(j, choice[j])]
    base = np.sqrt(np.mean(np.concatenate(resid) ** 2))
    for it in range(6):
        moved = 0
        for j in low:
            for r, s in zip(resid, sets):
                r += s[1][(j, choice[j])]
            best = min(keys, key=lambda c: sum(float(np.sum((r - s[1][(j, c)]) ** 2)) for r, s in zip(resid, sets)))
            moved += best != choice[j]
            choice[j] = best
            for r, s in zip(resid, sets):
                r -= s[1][(j, best)]
        if not moved:
            break
    final = np.sqrt(np.mean(np.concatenate(resid) ** 2))
    print('== ro %d -> %d seeds %s: residual %.2e (index carry) -> %.2e' % (old_cfg, new_cfg, SEEDS, base, final))
    for j in low:
        k, b, f, kd = tmpl.bands[j]
        i, cj = choice[j]
        src = 'none' if i is None else '%d%s (QMF %d f %+.3f par %d)' % (
            i, ' conj' if cj else '', old.bands[i][0], old.bands[i][2], old.bands[i][1])
        print('   new %2d (QMF %d f %+.3f par %2d) <- %s' % (j, k, f, b, src))


if __name__ == '__main__':
    for oc, nc in ((34, 20), (20, 34)):
        search(oc, nc)
