"""Recovery of the PS mixing from scratch (the mix recovery): crafts streams, decodes them with the
pinned reference, fits, and writes OUTDIR/mix.json. Deterministic; a few minutes on the box.

usage: recover_mix.py OUTDIR

Steps (each prints its evidence):
  A  D above QMF 23 in 20 bands: FIR taps of L with ICC index 7 (delays and gain).
  B  2x2 matrices for every IID x ICC index pair, first and second procedure, coarse and fine
     IID, fitted in parameter bands 18 / 19; then the IID level per index, rho per ICC index,
     the second procedure's rho floor, and the worst deviation of the closed forms.
  C  IPD/OPD over time (M column only, QMF 3..5): knot position of the linear ramps, phase
     step, and the smoothing weights (Gauss-Newton on every frame end after a change).
  D  Image hybrid sub-bands: per single sub-band ramps under OPD in all low bands, 20 and 34.
  E  IPD/OPD bands on the configuration (5/11/17 bands, 10/20 -> 34 maps), read back per
     hybrid sub-band and compared with the hyb recovery's maps.
  F  Whole-model checks on fresh streams (mixmodel.Mixer against the reference), and the same
     with single rules switched off; the 34-band streams with ICC also with the dec recovery's D.
  G  The same with the dec recovery's D in every band (ICC everywhere, IPD/OPD, header-less frames):
     the D column's phase and the image rule on h12/h22 (v_mix2).
"""
import json
import sys
from mixlib import *
import mixmodel as MM

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'hyb'))
import hybmodel as HM  # noqa: E402
from m26_hyb_ramps import low_bands  # noqa: E402
import v_mix  # noqa: E402
import v_mix2  # noqa: E402

OUT = sys.argv[1]
os.makedirs(OUT, exist_ok=True)
R = {}


def stream(name, pays, seed):
    p = os.path.join(OUT, name + '.aac')
    write(p, pays, seed=seed)
    y = decode(p)
    X, _ = dump(p)
    n = min(len(X), len(y) // 64)
    return y, X[:n], n


def golden(f, lo, hi, it=80):
    g = (np.sqrt(5) - 1) / 2
    a, b = lo, hi
    for _ in range(it):
        c, d = b - g * (b - a), a + g * (b - a)
        if f(c) < f(d):
            b = d
        else:
            a = c
    return (a + b) / 2


def ramps(Ts, X, seqs, n, nf, s0):
    """Free straight-line gain per frame for each (QMF band, sequence) -> (ch, item, frame, 2)."""
    j = (np.arange(n) % 32 + 1) / 32.0
    fr = np.arange(n) // 32
    out, rmss = [], []
    for T in Ts:
        terms = []
        for k, s in seqs:
            for m in range(2, nf):
                ind = fr == m
                terms += [(k, s * ind * (1 - j)), (k, s * ind * j)]
        a, rms, mx, sr = T.fit(terms, s0, nf * 32 - 8)
        out.append(a.reshape(len(seqs), nf - 2, 2))
        rmss.append(rms)
    return out, rmss


# ---------------------------------------------------------------- A: D above QMF 23
y, X, n = stream('a', [payload(iid=[0] * 20, icc=[0] * 18 + [7, 7])] * 12, 11)
T = Target(y[:, 0], zero_bands(X, range(23, 64)))
terms, lay = [], []
for k in range(23, 64):
    for d in ((12, 13, 14, 15, 16) if k < 35 else (-1, 0, 1, 2, 3)):
        terms.append((k, shift(X[:, k], d)))
        lay.append((k, d))
a, rms, mx, sr = T.fit(terms, 64, n - 12)
main = {}
for i, (k, d) in enumerate(lay):
    if k not in main or abs(a[i]) > abs(main[k][1]):
        main[k] = (d, a[i])
R['A_decorrelated_above_qmf23'] = {
    'delay_qmf_23_34': sorted({d for k, (d, v) in main.items() if k < 35}),
    'delay_qmf_35_63': sorted({d for k, (d, v) in main.items() if k >= 35}),
    'max_abs_gain_minus_1': float(max(abs(v - 1) for d, v in main.values())),
    'max_other_tap': float(max(abs(a[i]) for i, (k, d) in enumerate(lay) if d != main[k][0])),
    'fit_rms': rms, 'signal_rms': sr}
print('A', R['A_decorrelated_above_qmf23'], flush=True)

# ---------------------------------------------------------------- B: matrices
B18, B19 = range(23, 35), range(35, 64)


def fit_matrices(proc, grid):
    fine = grid == 'fine'
    iid_mode, icc_mode = (4 if fine else 1), (1 if proc == 'A' else 4)
    nmax = 15 if fine else 7
    pairs = [(i, j) for i in range(-nmax, nmax + 1) for j in range(8)]
    pays = [payload(iid=[0] * 20, icc=[0] * 20, iid_mode=iid_mode, icc_mode=icc_mode)] * 2
    for i, j in pairs:
        pays += [payload(iid=[0] * 18 + [i, i], icc=[0] * 18 + [j, j], iid_mode=iid_mode, icc_mode=icc_mode)] * 3
    y, X, n = stream('b', pays, 5)
    Ts = [Target(y[:, ch], zero_bands(X, range(23, 64))) for ch in (0, 1)]
    H, H19, worst = {}, {}, 0.0
    for c, ij in enumerate(pairs):
        f = 2 + 3 * c
        s0, s1 = 32 * (f + 1) + 10, 32 * (f + 3) - 4
        cols = []
        for bands, d in ((B18, 0), (B18, 14), (B19, 0), (B19, 1)):
            cols.append(sum(col(shift(X[:, k], d), k, s0, s1) for k in bands))
            cols.append(sum(col(1j * shift(X[:, k], d), k, s0, s1) for k in bands))
        A = np.array(cols).T
        hs = []
        for ch in (0, 1):
            yy = Ts[ch].y[64 * s0:64 * s1]
            co, *_ = np.linalg.lstsq(A, yy, rcond=None)
            worst = max(worst, float(np.sqrt(np.mean((yy - A @ co) ** 2))))
            hs.append(co[0::2] + 1j * co[1::2])
        H[ij] = np.array([hs[0][0], hs[0][1], hs[1][0], hs[1][1]])
        H19[ij] = np.array([hs[0][2], hs[0][3], hs[1][2], hs[1][3]])
    return H, H19, worst


def closed(proc, fine, i, rho, floor=True):
    """mixmodel's closed form for IID index i and an explicit rho."""
    save = list(MM.ICC_RHO), MM.RHO_MIN_B
    MM.ICC_RHO[1] = rho
    if not floor:
        MM.RHO_MIN_B = -2.0
    try:
        return MM.matrix_real([i], [1], fine, proc)[:, 0]
    finally:
        MM.ICC_RHO[:] = save[0]
        MM.RHO_MIN_B = save[1]


def eq7(fine, i, rho):
    cl, cr = (float(v[0]) for v in MM.iid_gains(np.array([i]), fine))
    al = np.arccos(rho) / 2
    be = np.arctan(np.tan(al) * (cr - cl) / (cr + cl))
    return np.array([cl * np.cos(be + al), cl * np.sin(be + al), cr * np.cos(be - al), cr * np.sin(be - al)])


RB = {}
for proc in ('A', 'B'):
    for grid in ('coarse', 'fine'):
        fine = grid == 'fine'
        H, H19, worst = fit_matrices(proc, grid)
        nmax = 15 if fine else 7
        # IID level per index from the ICC index 0 matrices: 20 log10(|row 1| / |row 2|)
        lv = [float(20 * np.log10(np.hypot(*H[(i, 0)][:2].real) / np.hypot(*H[(i, 0)][2:].real))) for i in range(nmax + 1)]
        power = max(abs(float(np.sum(H[k].real ** 2)) - 2) for k in H)
        rho = [1.0]
        for j in range(1, 8):
            keys = [k for k in H if k[1] == j]
            rho.append(float(golden(lambda r: sum(float(np.sum((closed(proc, fine, i, r, floor=False) - H[(i, jj)].real) ** 2))
                                                  for i, jj in keys), -1.0 + 1e-12, 1.0)))
        tab = MM.ICC_RHO
        dev = max(float(np.abs(MM.matrix_real([i], [j], fine, proc)[:, 0] - H[(i, j)].real).max()) for i, j in H)
        dev19 = max(float(np.abs(MM.matrix_real([i], [j], fine, proc)[:, 0] - H19[(i, j)].real).max()) for i, j in H)
        ent = {'fit_rms_worst': worst, 'max_imag': float(max(np.abs(h.imag).max() for h in H.values())),
               'iid_level_db_per_index': lv, 'max_power_minus_2': power, 'rho_fitted': rho,
               'model_max_abs_dev_band18': dev, 'model_max_abs_dev_band19': dev19}
        if proc == 'A':
            ent['eq7_max_abs_dev'] = max(float(np.abs(eq7(fine, i, tab[j]) - H[(i, j)].real).max()) for i, j in H if j)
        else:
            def f64form(i, j):
                c = 10.0 ** (float(MM.iid_db(np.array([i]), fine)[0]) / 20.0)
                r = max(tab[j], MM.RHO_MIN_B)
                a_ = np.pi / 4 if i == 0 else np.mod(0.5 * np.arctan(2 * c * r / (c * c - 1)), np.pi / 2)
                mu = np.sqrt(1 + (4 * r * r - 4) / (c + 1 / c) ** 2)
                g = np.arctan(np.sqrt((1 - mu) / (1 + mu)))
                return MM.SQ2 * np.array([np.cos(a_) * np.cos(g), -np.sin(a_) * np.sin(g), np.sin(a_) * np.cos(g), np.cos(a_) * np.sin(g)])
            ent['float64_mu_max_abs_dev'] = max(float(np.abs(f64form(i, j) - H[(i, j)].real).max()) for i, j in H)
        RB['%s_%s' % (proc, grid)] = ent
        print('B', proc, grid, {k: v for k, v in ent.items() if k not in ('iid_level_db_per_index',)}, flush=True)
        print('  IID level per index (dB):', ' '.join('%.5f' % v for v in lv), flush=True)
R['B_matrices'] = RB
rho_a = np.mean([RB['A_coarse']['rho_fitted'], RB['A_fine']['rho_fitted']], 0)
R['icc_rho'] = [float(v) for v in rho_a]
R['rho_floor_second_procedure'] = float(np.mean([RB['B_%s' % g]['rho_fitted'][j] for g in ('coarse', 'fine') for j in (5, 6, 7)]))

# ---------------------------------------------------------------- C: IPD/OPD over time
Z3 = ((0, 0), (0, 0), (0, 0))
S1 = ((0, 2), (3, 0), (5, 1))
sched = [Z3] * 4 + [S1] * 10 + [Z3] * 8 + [S1] + [Z3] * 6 + [((0, 4), (4, 0), (4, 4))] * 6 + [Z3] * 6
sched += [((0, v), (v, 0), (v, (8 - v) % 8)) for v in (1, 2, 3, 4, 5, 6, 7, 0)] + [Z3] * 6
sched += [((0, 1), (1, 0), (2, 6)), ((0, 5), (5, 0), (6, 2))] * 4 + [Z3] * 6


def phase_frame(s):
    ipd, opd = [0] * 11, [0] * 11
    for b, (i, o) in zip((8, 9, 10), s):
        ipd[b], opd[b] = i, o
    return payload(iid=[0] * 20, icc=[0] * 20, ext=ipdopd_ext(ipd, opd))


y, X, n = stream('c', [phase_frame(s) for s in sched], 21)
bands = [3, 4, 5]
Ts = [Target(y[:, ch], zero_bands(X, bands)) for ch in (0, 1)]
scan = {}
for delta in (-3, -2, -1, 0, 1):
    worst = 0
    for f0, f1 in ((3, 7), (13, 17), (40, 48)):
        for T in Ts:
            knots = [32 * m + delta for m in range(f0 - 1, f1 + 2)]
            worst = max(worst, knot_fit(T, X, bands, 32 * f0, 32 * f1, knots)[1])
    scan[delta] = worst
nf = n // 32
V = np.zeros((2, nf + 1, 3), complex)
for ch in (0, 1):
    for f0 in range(2, nf - 4, 2):
        ms = list(range(f0 - 1, f0 + 6))
        v, rms, mx, sr = knot_fit(Ts[ch], X, bands, 32 * f0, 32 * (f0 + 4), [32 * m - 1 for m in ms])
        for i, m in enumerate(ms):
            if f0 + 1 <= m <= f0 + 2 or (m == f0 and f0 == 2):
                V[ch, m] = v[i]
# measured phases at the end of frame f: knot m = f + 1
obs = []   # (frame, band, kind, measured phase)
for f in range(4, nf - 4):
    for b in range(3):
        oL = np.angle(V[0, f + 1, b])
        iL = np.angle(V[0, f + 1, b] / V[1, f + 1, b])
        obs.append((f, b, 'opd', oL))
        obs.append((f, b, 'ipd', iL))
mag = float(np.abs(np.abs(V[:, 5:nf - 4]) - 1).max())


def predict(w, f, b, kind):
    k = 1 if kind == 'opd' else 0
    z = sum(wi * np.exp(1j * sched[f - i][b][k] * MM.IPD_STEP) for i, wi in enumerate((1.0, w[0], w[1])) if f - i >= 0)
    return np.angle(z)


def resid(w):
    return np.array([np.angle(np.exp(1j * (predict(w, f, b, kd) - m))) for f, b, kd, m in obs])


w = np.array([0.4, 0.2])
for _ in range(20):
    r0 = resid(w)
    J = np.stack([(resid(w + dw) - r0) / 1e-7 for dw in (np.array([1e-7, 0]), np.array([0, 1e-7]))], 1)
    w = w - np.linalg.lstsq(J, r0, rcond=None)[0]
r_final = resid(w)
steady = [np.angle(V[0, 13, 0]), np.angle(V[0, 13, 2])]  # frame 12 end: opd 2 and 1 steady
R['C_phase_time'] = {'knot_delta_scan_worst_rms': {str(k): v for k, v in scan.items()},
                     'knot_at_slot': '32 m - 1 (last slot of each frame)',
                     'smoothing_weights_rel_current': [1.0, float(w[0]), float(w[1])],
                     'smoothing_fit_max_abs_phase_residual': float(np.abs(r_final).max()),
                     'smoothing_fit_rms_phase_residual': float(np.sqrt(np.mean(r_final ** 2))),
                     'knot_magnitude_max_dev_from_1': mag,
                     'phase_step_from_steady': [float(steady[0] / 2), float(steady[1] / 1)]}
print('C', R['C_phase_time'], flush=True)

# ---------------------------------------------------------------- D: image sub-bands
img = {}
for cfg, nbands in ((20, 8), (34, 16)):
    mode, nb = (1, 11) if cfg == 20 else (2, 17)
    opd = [2 if b < nbands else 0 for b in range(nb)]
    pay = payload(iid=[0] * NB[mode], icc=[0] * NB[mode], iid_mode=mode, icc_mode=mode, ext=ipdopd_ext([0] * nb, opd))
    y, X, n = stream('d%d' % cfg, [pay] * 12, 41)
    LB = low_bands(cfg, 'single')
    seqs = [(k, HM._fir(X[:, k], f)) for k, f, b, lab in LB]
    Ts = [Target(y[:, ch], zero_bands(X, range(HM.NLOW[cfg]))) for ch in (0, 1)]
    a, rmss = ramps(Ts, X, seqs, n, 12, 2 * 32 + 12)
    lst = []
    for i, (k, f, b, lab) in enumerate(LB):
        if b >= nbands:
            continue
        st, en = a[0][i, 5]                          # frame 7: start, end
        lst.append({'qmf': k, 'subband': int(lab.split(':')[1]), 'par': b,
                    'start_phase_over_pi4': float(np.angle(st) * 4 / np.pi),
                    'end_phase_over_pi4': float(np.angle(en) * 4 / np.pi)})
    found = sorted((d['qmf'], d['subband']) for d in lst if d['start_phase_over_pi4'] < -1)
    img[str(cfg)] = {'fit_rms': rmss, 'image_subbands': found, 'all': lst,
                     'matches_model': found == sorted(MM.IMAGE_SUBBANDS[cfg])}
    print('D', cfg, 'rms', rmss, 'image sub-bands', found, flush=True)
R['D_image_subbands'] = img

# ---------------------------------------------------------------- E: IPD/OPD bands on the configuration
emap = {}
CASES = {'20: IID mode 1 (11 bands)': (20, 1, 1, [(j * 3 + 1) % 8 for j in range(11)], [(j * 5 + 2) % 8 for j in range(11)]),
         '20: IID mode 0 (5 bands)': (20, 0, 0, [1, 7, 6, 3, 2], [3, 5, 2, 7, 1]),
         '34: IID mode 1 (11 bands)': (34, 1, 2, [1, 7, 6, 3, 2, 5, 0, 4, 1, 2, 3], [0, 5, 2, 7, 1, 1, 3, 6, 2, 0, 4]),
         '34: IID mode 0 (5 bands)': (34, 0, 2, [1, 7, 6, 3, 2], [3, 5, 2, 7, 1]),
         '34: IID mode 2 (17 bands)': (34, 2, 2, [(j * 3 + 1) % 8 for j in range(17)], [(j * 5 + 2) % 8 for j in range(17)])}
for name, (cfg, imode, cmode, ipd, opd) in CASES.items():
    pay = payload(iid=[0] * NB[imode], icc=[0] * NB[cmode], iid_mode=imode, icc_mode=cmode, ext=ipdopd_ext(ipd, opd))
    y, X, n = stream('e', [pay] * 10, 41)
    LB = low_bands(cfg, 'single') + [(k, None, b, str(k)) for k, b in sorted(HM.qmf_par(cfg).items()) if k < 8]
    seqs = [(k, X[:, k] if f is None else HM._fir(X[:, k], f)) for k, f, b, lab in LB]
    Ts = [Target(y[:, ch], zero_bands(X, range(8))) for ch in (0, 1)]
    a, rmss = ramps(Ts, X, seqs, n, 10, 2 * 32 + 12)
    nb_iid = NB[imode]
    want_i = MM.to_config_ipd(ipd, nb_iid, cfg, HM)
    want_o = MM.to_config_ipd(opd, nb_iid, cfg, HM)
    worst = 0.0
    for i, (k, f, b, lab) in enumerate(LB):
        gl, gr = a[0][i, 6, 1], a[1][i, 6, 1]                 # frame 8 end
        e1 = abs(np.angle(np.exp(1j * (np.angle(gl) - want_o[b] * MM.IPD_STEP))))
        e2 = abs(np.angle(np.exp(1j * (np.angle(gl / gr) - want_i[b] * MM.IPD_STEP))))
        worst = max(worst, e1, e2)
    emap[name] = {'fit_rms': rmss, 'max_phase_error_vs_maps_rad': float(worst)}
    print('E', name, emap[name], flush=True)
R['E_ipd_bands'] = emap
R['ipd_band_count_by_iid_bands'] = MM.NIPD

# ---------------------------------------------------------------- F: whole model
checks = {}
for name, args, kw in (('A 20 bands first procedure', (77, 1, 1), {}),
                       ('B 20 bands second procedure fine', (78, 4, 4), {}),
                       ('D34 20->34 second procedure fine', (80, 4, 5), {'cfg': 34}),
                       ('C34 34 bands, ICC 0', (79, 2, 2), {'cfg': 34, 'use_icc': False}),
                       ('E34 10->34, ICC 0', (81, 0, 2), {'cfg': 34, 'use_icc': False}),
                       ('C34 34 bands, ICC, dec D', (79, 2, 2), {'cfg': 34, 'dec': True}),
                       ('E34 10->34, ICC, dec D', (81, 0, 2), {'cfg': 34, 'dec': True})):
    checks[name] = v_mix.check(name, *args, outdir=OUT, quiet=True, **kw)
for name, flag in (('without off_clears', 'off_clears'), ('without image rule', 'image'),
                   ('without stale imaginary part', 'stale_im'), ('without per-envelope reuse', 'per_env')):
    for sname, args, kw in (('B', (78, 4, 4), {}), ('D34', (80, 4, 5), {'cfg': 34})):
        checks['%s: %s' % (sname, name)] = v_mix.check(sname + ' ' + name, *args, outdir=OUT, quiet=True,
                                                         mixer_kw={flag: False}, **kw)
R['F_model_checks_rms_max_signal'] = checks

# ---------------------------------------------------------------- G: with the dec recovery's D
R['G_dec_D_checks_rms_max_signal'] = {c[0]: v_mix2.check(*c, OUT, variants=v_mix2.VARIANTS) for c in v_mix2.CASES}

R['tables'] = {'iid_grid_coarse_db': MM.IID_COARSE, 'iid_grid_fine_db': MM.IID_FINE,
               'icc_rho_model': MM.ICC_RHO, 'rho_floor_model': MM.RHO_MIN_B,
               'phase_step_rad': MM.IPD_STEP, 'smoothing_weights_model': MM.SMOOTH_W,
               'image_subbands_model': {str(k): sorted(v) for k, v in MM.IMAGE_SUBBANDS.items()}}
json.dump(R, open(os.path.join(OUT, 'mix.json'), 'w'), indent=1, default=lambda o: o.tolist() if hasattr(o, 'tolist') else str(o))
print('wrote', os.path.join(OUT, 'mix.json'))
