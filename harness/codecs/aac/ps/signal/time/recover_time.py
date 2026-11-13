"""Recovery of the time recovery from scratch: crafts streams, decodes them with the pinned
reference, fits per-slot gains (and, with the models of the other parts composed, start matrices),
and writes OUTDIR/time.json with the recovered tables and the evidence for each rule of
timemodel.py.

usage: recover_time.py OUTDIR

Recovered here:
- envelope borders of frame_class 0 (1, 2, 4 envelopes) and frame_class 1 (coded values,
  extra envelope to slot 31), and the per-slot interpolation weights;
- the start of the first PS frame (zero matrix);
- the conversion of the start matrix between the 20- and 34-band layouts, every band;
- the state over a 20 <-> 34 change (decorrelator, ducking, hybrid history) and the
  configuration of a header without IID and ICC;
- dt coding and num_env 0 across a change of band count;
- a verification of timemodel.py on fresh random streams: IID/ICC-only predictions and
  the composed whole model (compose.py)."""
import itertools
import json
import sys

from timelib import *
import timemodel as TM
import compose as C

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
res = {'notes': 'rules: harness/codecs/aac/ps/README.md'}
CLc = {v: float(cl_cr(iid_db(v))[0]) for v in range(-7, 8)}


def whole_row(path, spec, seed=3, bins=None):
    build2(path, spec, seed=seed, bins=bins)
    y, msgs, _ = decode_log(path)
    X, _ = dump(path)
    n = len(X)
    g, rms, mx, _ = slot_gain_fit(y[:, 0], X, range(64), range(n), 0, 64 * (n - 11), real=True)
    return g, rms, msgs


def P(*v, fc=0, borders=()):
    return pdata(iid=[[x] * 20 for x in v], fc=fc, borders=borders)


# --- 1. borders and interpolation ---------------------------------------------------------
frames = [((7,), 0, ()), ((-7, 3), 0, ()), ((5, -5, 2, -2), 0, ()), ((-3,), 1, (11,)),
          ((6, -6, 1), 1, (4, 9, 31)), ((0, 4), 1, (0, 20))]
spec = [P(0)] + [P(*v, fc=fc, borders=b) for v, fc, b in frames] + [P(0)]
g, rms, msgs = whole_row(os.path.join(out, 'borders.aac'), spec)
borders_found, weight_dev = [], 0.0
prev = CLc[0]
for i, (v, fc, b) in enumerate(frames):
    seg = g[32 * (i + 1):32 * (i + 2)]
    found, first, start = [], 0, prev
    for val in v:
        T = CLc[val]
        hit = [t for t in range(first, 32) if abs(seg[t] - T) < 2e-6]
        end = hit[0]
        found.append(end)
        w = (seg[first:end + 1] - start) / (T - start)
        ideal = (np.arange(first, end + 1) - first + 1) / (end - first + 1)
        weight_dev = max(weight_dev, float(np.abs(w - ideal).max()))
        first, start = end + 1, T
    hold = bool(np.all(np.abs(seg[first:] - CLc[v[-1]]) < 2e-6))
    borders_found.append({'nenv': len(v), 'frame_class': fc, 'coded': list(b), 'last_slots': found,
                          'held_to_31': hold})
    prev = CLc[v[-1]]
res['borders'] = {
    'measured': borders_found,
    'rule_frame_class_0': 'envelope e of n ends at slot 32*(e+1)/n - 1',
    'rule_frame_class_1': 'envelope e ends at its coded border; after the last border the last target holds',
    'interpolation': 'h(t) = h_prev + (h_target - h_prev) * (t - a + 1) / (b - a + 1) for slots a..b',
    'max_weight_deviation': weight_dev,
    'fit_rms': rms,
}
ok_fc0 = all(d['last_slots'] == [32 * (e + 1) // d['nenv'] - 1 for e in range(d['nenv'])]
             for d in borders_found if d['frame_class'] == 0)
ok_fc1 = all(d['last_slots'] == d['coded'] and d['held_to_31'] for d in borders_found if d['frame_class'] == 1)
res['borders']['rules_hold'] = bool(ok_fc0 and ok_fc1)
print('borders', borders_found, 'weight dev %.2g' % weight_dev, 'rules hold', ok_fc0, ok_fc1)

# --- 2. first PS frame: start of the ramp -------------------------------------------------
g, rms, _ = whole_row(os.path.join(out, 'first.aac'), [None, None, P(7), P(7)])
seg = g[64:96]
start = float((seg[0] - CLc[7] / 32) * 32 / 31)
res['first_frame'] = {'start_value_L_gain': start, 'frames_before_first_PS_gain': float(np.abs(g[8:60] - 1).max()),
                      'rule': 'the first PS frame ramps from the zero matrix; frames before it are mono'}
print('first frame start %.2g' % start)

# --- 3. 20 <-> 34: start conversion over every parameter band (as t25) ------------------


def clr(i):
    return cl_cr(FINE[i + 15])


def fit_change(name, mode_a, vals, mode_b, seed=3):
    """Per parameter band of the new configuration: fitted start (cl, cr) of the change
    frame (IID only, fine grid, one index -15 after the change), on the composed model."""
    cfg_b = C.HM.config(mode_b, None)
    nb_b = NB[mode_b]
    spec = [pdata(iid=[vals], iid_mode=mode_a)] * 2 + [pdata(iid=[[-15] * nb_b], iid_mode=mode_b)] * 3

    def mk(mode, v):
        return {'ps': True, 'header': {'iid_mode': mode, 'icc_mode': None}, 'frame_class': 0, 'nenv': 1,
                'cfg': C.HM.config(mode, None), 'iid': [(False, list(np.diff([0] + list(v))))], 'icc': None}

    model = [mk(mode_a, vals)] * 2 + [mk(mode_b, [-15] * nb_b)] * 3
    p = os.path.join(out, name + '.aac')
    build2(p, spec, seed=seed)
    y, _, _ = decode_log(p)
    X, _ = dump(p)
    comp = C.Composer()
    orig = comp.tl._start
    comp.tl._start = lambda cfg, old=None: np.zeros((cfg, 4)) if old is not None else orig(cfg, old)
    L, R = comp.run(X, model)
    win = np.zeros((44, 64), complex)
    win[:6] = X[58:64]
    win[6:44] = X[64:102]
    Hm = C.DM.analysis(win, cfg_b)[6:38]
    par = np.array([b for _, b, _, _ in C.DM.bands(cfg_b)])
    w = (np.arange(32) + 1) / 32.0
    n = len(X)
    m = 64 * (n - 11)
    cols = []
    for b in range(cfg_b):
        Q = np.zeros((n, 64), complex)
        Q[64:96] = C.DM.to_qmf(np.where(par[None, :] == b, Hm, 0) * (1 - w)[:, None], cfg_b)
        cols.append(psmodel.synthesis(Q)[:m])
    A = np.array(cols).T
    st, rr_ = [], []
    for ch, Y in ((0, L), (1, R)):
        rr = y[:m, ch] - psmodel.synthesis(Y)[:m]
        a, *_ = np.linalg.lstsq(A, rr, rcond=None)
        st.append(a)
        rr_.append(float(np.sqrt(np.mean((rr - A @ a) ** 2))))
    return np.array(st).T, rr_


v20 = [-15, -12, -9, -6, -3, 0, 3, 6, 9, 12, 15, -14, -11, -8, -5, -2, 1, 4, 7, 10]
s, rms20 = fit_change('c20_34', 4, v20, 5)
stored = np.array([clr(v) for v in v20])
t2034, e2034 = {}, 0.0
for j in range(34):
    cands = [((b,), stored[b]) for b in range(20)] + \
            [((b0, b1), stored[[b0, b1]].mean(0)) for b0, b1 in itertools.combinations(range(20), 2)]
    src, val = min(cands, key=lambda c: np.abs(c[1] - s[j]).sum())
    t2034[j] = src
    e2034 = max(e2034, float(np.abs(val - s[j]).max()))
found, e3420 = [], 0.0
for perm in (0, 1):
    rng = np.random.default_rng(11 + perm)
    v34 = [int(v) for v in rng.permutation(np.arange(-15, 16))] + [int(v) for v in rng.integers(-15, 16, 3)]
    s, rms34 = fit_change('c34_20_%d' % perm, 5, v34, 4)
    stored = np.array([clr(v) for v in v34])
    tab = {}
    for b in range(20):
        best = None
        for wts in itertools.product((0, 1, 2), repeat=10):   # weighted means of j0..9
            wt = np.array(wts, float)
            if wt.sum():
                val = (wt[:, None] * stored[:10]).sum(0) / wt.sum()
                e = np.abs(val - s[b]).sum()
                if best is None or e < best[0]:
                    best = (e, tuple(j for j in range(10) for _ in range(wts[j])), val)
        for size in (1, 2, 3, 4):                               # runs of j
            for j0 in range(34 - size + 1):
                js = tuple(range(j0, j0 + size))
                val = stored[list(js)].mean(0)
                e = np.abs(val - s[b]).sum()
                if e < best[0]:
                    best = (e, js, val)
        tab[b] = best[1]
        if perm == 1:
            e3420 = max(e3420, float(np.abs(best[2] - s[b]).max()))
    found.append(tab)
t3420 = {b: found[1][b] for b in range(20)}
res['conversion'] = {
    'fit_rms': {'20_34': rms20, '34_20': rms34},
    'copy_or_mean_20_to_34': {str(j): v for j, v in t2034.items()}, 'max_err_20_34': e2034,
    'mean_34_to_20': {str(b): v for b, v in t3420.items()}, 'max_err_34_20': e3420,
    'bands_where_permutations_disagree': [b for b in range(20) if found[0][b] != found[1][b]],
    'model_tables_agree': bool(all(tuple(TM.CONV_20_TO_34[j]) == t2034[j] for j in range(34)) and
                               all(tuple(TM.CONV_34_TO_20[b]) == v for b, v in t3420.items())),
    'rule': ('a frame whose configuration differs from that of the frame read before it starts from '
             'the stored target converted entry by entry: 20 -> 34 copies (two entry means), 34 -> 20 '
             'means (b0..b3 weighted 2:1); otherwise the stored rows are read as they are'),
}
print('conversion: 20->34 err %.2g, 34->20 err %.2g, agrees with timemodel: %s, disagreeing bands %s' % (
    e2034, e3420, res['conversion']['model_tables_agree'], res['conversion']['bands_where_permutations_disagree']))

# --- 4. state over a 20 <-> 34 change (as t26) and a header without IID / ICC (t27) -----


def q7(mode):
    return pdata(icc=[[7] * NB[mode]], icc_mode=mode)


def m7(mode, cfg=None):
    if mode is None:
        return {'ps': True, 'header': {'iid_mode': None, 'icc_mode': None}, 'frame_class': 0, 'nenv': 1,
                'cfg': cfg, 'iid': None, 'icc': None}
    return {'ps': True, 'header': {'iid_mode': None, 'icc_mode': mode}, 'frame_class': 0, 'nenv': 1,
            'cfg': C.HM.config(None, mode), 'iid': None, 'icc': [(False, [7] + [0] * (NB[mode] - 1))]}


def frame_res(spec, model, pol, frames):
    p = os.path.join(out, 'state.aac')
    build2(p, spec)
    y, _, _ = decode_log(p)
    X, _ = dump(p)
    L, R = C.Composer(pol).run(X, model)
    d = y[:, 0] - psmodel.synthesis(L)[:len(y)]
    return max(float(np.sqrt(np.mean(d[2048 * f:2048 * f + 2048] ** 2))) for f in frames)


state = {}
for a, b in ((1, 2), (2, 1)):
    seq = [a] * 3 + [b] * 3 + [a] * 3
    for name, pol in (('reset', {}), ('carry_by_qmf_band', {'dec': 'qmf', 'duck': 'index'}),
                      ('carry_by_index', {'dec': 'index', 'duck': 'index'}),
                      ('separate_per_configuration', {'dec': 'separate'}),
                      ('hybrid_history_zero', {'hist': 'zero'})):
        state['%d_%d_%s' % (NB[a], NB[b], name)] = frame_res([q7(m) for m in seq], [m7(m) for m in seq], pol,
                                                            range(3, 8))
for h0 in (34, 20):
    state['h0_in_34_runs_%d' % h0] = frame_res([q7(2)] * 3 + [S.header() + S.grid(0, 1)] + [q7(2)] * 3,
                                               [m7(2)] * 3 + [m7(None, h0)] + [m7(2)] * 3, {}, range(3, 6))
res['change_state'] = {'residual_of_frames_after_the_change': state,
                       'rule': 'decorrelator and ducking start from zero at a change; hybrid history kept; '
                               'a header with IID and ICC disabled keeps the configuration'}
print('change state', {k: '%.1e' % v for k, v in state.items()})

# --- 5. dt and num_env 0 across a change of band count (as t29) --------------------------
bc = {}
for kind in ('iid', 'icc'):
    name = kind.upper()
    for a, b in ((0, 1), (1, 0), (1, 2), (2, 1)):
        for how in ('dt', 'n0'):
            rng = np.random.default_rng(7 * a + b)
            lo, hi = (-7, 7) if kind == 'iid' else (0, 7)
            v = [int(x) for x in np.clip(rng.integers(lo, hi + 1) + np.cumsum(rng.integers(-3, 4, NB[a])), lo, hi)]

            def fr(mode, coded_bits, coded, nenv):
                m = {'ps': True, 'header': {'iid_mode': mode if kind == 'iid' else None,
                                            'icc_mode': mode if kind == 'icc' else None},
                     'frame_class': 0, 'nenv': nenv, 'iid': None, 'icc': None,
                     'cfg': C.HM.config(mode if kind == 'iid' else None, mode if kind == 'icc' else None)}
                m[kind] = coded
                return S.header(**{kind: mode}) + S.grid(0, nenv) + coded_bits, m

            f0 = fr(a, '0' + df_codes(v, name + '_DF'), [(False, list(np.diff([0] + v)))], 1)
            if how == 'dt':
                f1 = fr(b, '1' + B[name + '_DT'][0] * NB[b], [(True, [0] * NB[b])], 1)
            else:
                f1 = fr(b, '', [], 0)
            p = os.path.join(out, 'bc.aac')
            build2(p, [f0[0], f0[0], f1[0], None, None])
            y, _, _ = decode_log(p)
            X, _ = dump(p)
            for rule in ('index', 'resample'):
                Lm, Rm = C.Composer({'bandmap': rule}).run(X, [f0[1], f0[1], f1[1], {'ps': False}, {'ps': False}])
                e = max(float(np.sqrt(np.mean((y[2048 * 2:2048 * 4, c] - psmodel.synthesis(Y)[2048 * 2:2048 * 4]) ** 2)))
                        for c, Y in ((0, Lm), (1, Rm)))
                bc['%s_%d_%d_%s_%s' % (kind, NB[a], NB[b], how, rule)] = e
res['band_count_change'] = {'residual': bc, 'rule': ('kept values are one array of 34 per parameter: a frame '
                                                     'writes its first n entries; dt and num_env 0 read the '
                                                     'first n entries of the new count')}
print('band count', {k: '%.1e' % v for k, v in bc.items()})

# --- 6. verification on fresh random streams ---------------------------------------------
import verify_time as V  # noqa: E402
import verify_compose as VC  # noqa: E402
ver = {}
for label, seed0, count, psw, start_cfg in (('20', 7001, 2, 0.0, 20), ('34', 7101, 1, 0.0, 34),
                                             ('switch', 7201, 2, 0.15, 20)):
    V.P_SWITCH, V.START_CFG = psw, start_cfg
    ver['iid_icc_only_' + label] = V.main(out, seed0, count)
for seed, psw, sc in ((7301, 0.0, 20), (7302, 0.0, 34), (7303, 0.1, 20), (7304, 0.1, 34)):
    w, bad, _, _ = VC.run(out, seed, psw, sc, None, True)
    ver['composed_%d' % seed] = w
res['verification_worst_rms'] = ver
res['rules'] = [
    'matrix entries (not parameters) are interpolated linearly per slot (t03, t07)',
    'first PS frame of a stream ramps from the zero matrix; earlier frames are mono (t01, t02)',
    'frame without PS: repeats the last PS frame (envelopes, targets); start = stored target '
    'n_before-1 (t05, t06, t07); after an error frame: mono, no state advance (t09, t11)',
    'num_env 0: one envelope to slot 31, target = last envelope values of the previous frame '
    '(index 0 initially), counts as 1 envelope (t04, t07)',
    'header without IID (ICC): those values become index 0 (t04, t07); with both disabled the '
    'configuration is kept (t27)',
    'error frames (illegal index, dt result out of range, reserved mode, decreasing border, '
    'size mismatch, num_env 0 with fine values on the coarse grid): mono output, no PS state '
    'advances, stored values reset to 0, header-less frames ignored until the next header '
    '(t08, t09, t11, t12, t17, t18, t20..t23)',
    'frame_class 1 with last border < 31 gets one more envelope to 31 (t21, t23, verify)',
    'hybrid history: the 6 slots before a frame come from the last frame the PS ran (zero at '
    'the first PS frame) (t16); decorrelator state is zero at the first PS frame and kept '
    'unchanged over frames the PS does not run (t10, t11)',
    '20 <-> 34 change: start converted (t14, t25), decorrelator and ducking from zero, hybrid '
    'history kept (t26); a frame without PS repeating a change frame repeats the change '
    '(verify_compose)',
    'kept values across band-count changes: first n entries of a 34-entry array (t29, verify_compose)',
    'store index -1 = start matrix of the last frame that ran (read at count 0); a refused '
    'num_env 0 frame counts 0; the configuration state starts at 20 bands; a header-less '
    'size-mismatch frame counts for the configuration (t31, v_rust)',
    'failed or missing SBR element: the high band is zero in that frame, the PS repeats (t15)',
]
with open(os.path.join(out, 'time.json'), 'w') as fh:
    json.dump(res, fh, indent=1, default=lambda o: sorted(o) if isinstance(o, set) else str(o))
print('wrote', os.path.join(out, 'time.json'))
