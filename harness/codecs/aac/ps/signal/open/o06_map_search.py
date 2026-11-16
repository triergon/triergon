"""o06 (case 1): which old band's decorrelator state each band of the new layout starts
from after a change registered while off. On o01's ro streams (ICC 7, L = D), with the
ducker's state carried by parameter-band index (o04, o05: QMF >= 24 then match at 1e-10)
and every per-band array carried by band index as the starting point, a coordinate
descent picks for every all-pass band of the new layout (low hybrid bands and QMF bands
below 23) the old band (any of the old layout's) whose state, carried into the new band,
minimises the time-domain residual of L over the corner frame and the next. Run for each
all-pass realization (FormDecorrelator).

usage: o06_map_search.py OUTDIR [forms]"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')
FORMS = sys.argv[2].split(',') if len(sys.argv) > 2 else ['lat']
NF = 2          # frames compared from the corner on


def carry_all(old, new, i, conj=False):
    """new's per-band arrays all from old band i (complex conjugated with conj)."""
    cj = np.conj if conj else (lambda v: v)
    for jn in range(len(new.bands)):
        new.hin[:, jn] = cj(old.hin[:, i])
        for m in range(3):
            new.sx[m][:, jn] = cj(old.sx[m][:, i])
            new.sy[m][:, jn] = cj(old.sy[m][:, i])
            new.w[m][:, jn] = cj(old.w[m][:, i])
            if old.kind[i] != 0:      # a band that did not run the all-pass (o05)
                new.sx[m][:, jn] = new.sy[m][:, jn] = new.w[m][:, jn] = 0


def search(old_cfg, new_cfg, form, corner=6):
    frames = O1.stream('ro', old_cfg, new_cfg)
    p = os.path.join(src, 'o01_ro_%d_%d.aac' % (old_cfg, new_cfg))
    model = [f[1] for f in frames]
    y, msgs, _ = decode_log(p)
    X, _ = dump(p)
    comp = CarryComposer({'form': form, 'carry': 'index', 'duck': 'index', 'zero': 'oldplain'})
    L, R = comp.run(X, model)
    old = comp.old_dec
    runs = [f for f in range(corner, len(comp.outs)) if comp.outs[f].processed]
    runs = runs[:NF + 1]
    Hc = np.concatenate([comp.trace[f][0] for f in runs])
    Dbase = np.concatenate([comp.trace[f][1] for f in runs])
    s0 = 32 * corner
    n = len(Hc)
    tmpl = FormDecorrelator(new_cfg, form)
    ap = [j for j in range(len(tmpl.bands)) if tmpl.kind[j] == 0]
    # D of every new band started from each old band's state
    Dc = {}
    for i in [(i, False) for i in range(len(old.bands))] + [(i, True) for i in range(len(old.bands)) if old.kind[i] == 0]:
        dec = FormDecorrelator(new_cfg, form)
        carry_all(old, dec, i[0], i[1])
        n0 = min(old.npar, dec.npar)
        dec.pk[:n0], dec.e1[:n0], dec.e2[:n0] = old.pk[:n0], old.e1[:n0], old.e2[:n0]
        Dc[i] = dec.process(Hc)
    lo, hi = 64 * s0, 64 * (s0 + 32 * NF)
    r = (y[:, 0] - psmodel.synthesis(L)[:len(y)])[lo:hi]
    base = float(np.sqrt(np.mean(r ** 2)))

    def delta(j, i):
        k = tmpl.bands[j][0]
        seq = Dc[i][:, j] - Dbase[:, j]
        full = np.zeros(s0 + n, complex)
        full[s0:] = seq
        return psmodel.synth_band(full, k)[lo:hi]

    D = {(j, i): delta(j, i) for j in ap for i in Dc}
    choice = {j: (j, False) for j in ap}       # the index carry the model started from
    cur = {j: D[(j, (j, False))] for j in ap}
    resid = r.copy()
    for it in range(4):
        moved = 0
        for j in ap:
            resid += cur[j]
            best = min(Dc, key=lambda i: float(np.sum((resid - D[(j, i)]) ** 2)))
            if best != choice[j]:
                moved += 1
            choice[j], cur[j] = best, D[(j, best)]
            resid -= cur[j]
        if not moved:
            break
    final = float(np.sqrt(np.mean(resid ** 2)))
    print('== ro %d -> %d form %s: residual %.2e (index carry) -> %.2e after %d passes' % (
        old_cfg, new_cfg, form, base, final, it + 1))
    for j in ap:
        if tmpl.bands[j][3] != 'low':
            continue
        k, b, f, kd = tmpl.bands[j]
        ko, bo, fo, kdo = old.bands[choice[j][0]]
        print('   new %2d (QMF %2d %s f %+.3f par %2d) <- old %2d%s (QMF %2d %s f %+.3f par %2d)%s' % (
            j, k, kd, f, b, choice[j][0], ' conj' if choice[j][1] else '', ko, kdo, fo, bo,
            '' if choice[j] == (j, False) else '  *'))
    return choice, final


if __name__ == '__main__':
    for form in FORMS:
        for oc, nc in ((34, 20), (20, 34)):
            search(oc, nc, form)
