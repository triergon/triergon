"""o07 (case 1): the decorrelator state the low hybrid bands start from after a change
registered while off, fitted freely. On o01's ro streams (ICC 7, L = D) the model carries
every other band's state (lattice sections, o05) and starts the low hybrid bands from
zero; the residual of L over the corner frame and the next two is then fitted as the
zero-input response of an unknown state per low band (the 2 input-history values the
lead delay reads and the 3 + 4 + 5 lattice values), times the ducking gain. The fitted
states are compared with the old layout's band states (each old low band, as stored and
conjugated).

usage: o07_state_fit.py OUTDIR"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')
NF = 3
FORM = 'lat'
ELEMS = [('hin', 0, 0), ('hin', 0, 1)] + [('w', m, i) for m, d in enumerate(C.DM.DELAYS) for i in range(d)]


def state_vec(dec, j):
    return np.array([dec.hin[i, j] if kind == 'hin' else dec.w[m][i, j] for kind, m, i in ELEMS])


def fit(old_cfg, new_cfg, corner=6):
    frames = O1.stream('ro', old_cfg, new_cfg)
    p = os.path.join(src, 'o01_ro_%d_%d.aac' % (old_cfg, new_cfg))
    model = [f[1] for f in frames]
    y, msgs, _ = decode_log(p)
    X, _ = dump(p)
    comp = CarryComposer({'form': FORM, 'carry': 'index', 'duck': 'index', 'zero': 'oldplain+low'})
    L, R = comp.run(X, model)
    old = comp.old_dec
    runs = [f for f in range(corner, len(comp.outs)) if comp.outs[f].processed][:NF]
    tmpl = FormDecorrelator(new_cfg, FORM)
    low = [j for j, b in enumerate(tmpl.bands) if b[3] == 'low']
    Hm = np.concatenate([comp.trace[f][0] for f in runs])
    n = len(Hm)
    s0 = 32 * corner
    lo, hi = 64 * s0, 64 * (s0 + 32 * NF)
    r = (y[:, 0] - psmodel.synthesis(L)[:len(y)])[lo:hi]
    # ducking gains of these frames: the ducker started from the carried state
    dk = FormDecorrelator(new_cfg, FORM)
    n0 = min(old.npar, dk.npar)
    dk.pk[:n0], dk.e1[:n0], dk.e2[:n0] = old.pk[:n0], old.e1[:n0], old.e2[:n0]
    dk.process(Hm)
    g = dk.glog
    cols, names = [], []
    for e, (kind, m, i) in enumerate(ELEMS):
        z = FormDecorrelator(new_cfg, FORM)
        if kind == 'hin':
            z.hin[i, low] = 1
        else:
            z.w[m][i, low] = 1
        Z = z.process(np.zeros((n, len(z.bands)), complex))   # zero input: gain 1
        for j in low:
            k = tmpl.bands[j][0]
            seq = Z[:, j] * g[:, tmpl.par[j]]
            for c in (1.0, 1j):
                full = np.zeros(s0 + n, complex)
                full[s0:] = c * seq
                cols.append(psmodel.synth_band(full, k)[lo:hi])
            names.append((j, e))
    A = np.array(cols).T
    a, *_ = np.linalg.lstsq(A, r, rcond=None)
    res = r - A @ a
    print('== ro %d -> %d: residual %.2e -> %.2e (free low-band states, %d unknowns)' % (
        old_cfg, new_cfg, np.sqrt(np.mean(r ** 2)), np.sqrt(np.mean(res ** 2)), A.shape[1]))
    st = {}
    for (j, e), c in zip(names, a[0::2] + 1j * a[1::2]):
        st.setdefault(j, np.zeros(len(ELEMS), complex))[e] = c
    olow = [i for i, b in enumerate(old.bands) if b[3] == 'low' or old.kind[i] == 0]
    for j in low:
        v = st[j]
        best = []
        for i in olow:
            for conj in (False, True):
                u = state_vec(old, i)
                u = u.conj() if conj else u
                best.append((np.linalg.norm(v - u) / (np.linalg.norm(v) + 1e-30), i, conj))
        best.sort()
        k, b, f, kd = tmpl.bands[j]
        print('   new %2d (QMF %d f %+.3f): |state| %.2e  nearest old: %s' % (
            j, k, f, np.linalg.norm(v),
            ', '.join('%d%s (QMF %d f %+.3f) %.1e' % (i, '*' if cj else '', old.bands[i][0], old.bands[i][2], d)
                      for d, i, cj in best[:3])))
    for j in (6, 7, 8, 9) if new_cfg == 20 else (20, 21, 24, 28):
        print('   new %d fitted vs old %d per element (hin x[-1], x[-2], then sections):' % (j, j))
        v, u = st[j], state_vec(old, j)
        for e, (kind, m, i) in enumerate(ELEMS):
            print('     %s%d[%d] %+.3e%+.3ej   %+.3e%+.3ej   ratio %s' % (
                kind, m, i, v[e].real, v[e].imag, u[e].real, u[e].imag,
                '%.3f@%+.1f' % (abs(v[e] / u[e]), np.degrees(np.angle(v[e] / u[e]))) if abs(u[e]) > 0 else '-'))
    return st, old


if __name__ == '__main__':
    for oc, nc in ((34, 20), (20, 34)):
        fit(oc, nc)
