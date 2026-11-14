"""recover_hyb.py OUTDIR: reruns the whole `hyb` recovery against the pinned reference and
writes OUTDIR/hyb.json. Deterministic (fixed seeds). recover_tables.py runs it with
`signal` on PYTHONPATH and FFMPEG and PS_INPUT in the environment.

Steps (each prints its evidence):
 A. layout: one one-hot IID stream per parameter band (mode 1 and mode 2: 20 and 34 bands,
    band b at +25 dB, the rest 0 dB). (L - R) / (g_L - g_R) is the band-b part of X; it is
    fitted as an FIR (taps -6..6) on QMF bands 0..5 and a gain on bands 6..63. A gain of 1
    (or a unit-impulse FIR) puts the QMF band in b; any other FIR is a hybrid part. For each
    split QMF band, the sub-band count Q and each part's sub-bands S are the ones whose
    responses |H_b(w_m)| at w_m = (2m+1) pi / Q partition the Q sub-bands most cleanly.
 B. prototypes: per configuration, one one-hot stream per parameter band with hybrid parts,
    the core spectrum restricted to the split QMF bands and one more, 32 frames; L - R is
    fitted with the real symmetric prototypes g_Q(6..12) as unknowns (normal equations over
    all streams). Taps no output depends on are reported as null.
 C. configuration: every IID/ICC mode combination (off, 0..5 each); an IID pattern (ICC 0)
    or ICC pattern stream is fitted with per-parameter-band gains under both layouts; the
    one with the float-level residual is the configuration run.
 D. maps of 10 and 20 coded bands onto the configuration run: three random-pattern streams
    per case (IID fine grid, ICC), read back per configured band as an index; each
    configured band is explained as a copy of one coded band or as a mean of two (rules:
    truncation toward zero, floor, rounding half away from zero).
 E. verification on fresh streams with hybmodel and with the recovered values.
"""
import json
import sys
import time
from hyblib import *
from fitlib import group_fit
import hybmodel as hm

OUT = sys.argv[1]
T0 = time.time()
R = {}
cl7, cr7 = gains(7)
DD = np.arange(-6, 7)


def log(*a):
    print('[%5.0fs]' % (time.time() - T0), *a)
    sys.stdout.flush()


def mods(Q, S):
    return sum(np.exp(1j * (2 * m + 1) * np.pi / Q * DD) for m in S)


# ---------------------------------------------------------------- A. layout
def layout(mode, nb, seed0):
    parts = {}   # (k, b) -> FIR taps -6..6
    plain = {}   # k -> b
    for b in range(nb):
        idx = [0] * nb
        idx[b] = 7
        p = os.path.join(OUT, 'A%d_%02d.aac' % (mode, b))
        write(p, [payload(iid=idx, iid_mode=mode)] * 8, seed=seed0 + b)
        y = decode(p)
        X, _ = dump(p)
        d = (y[:, 0] - y[:, 1]) / (cl7 - cr7)
        terms = []
        for k in range(6):
            terms += [(k, s) for s in taps(X[:, k], -6, 6)]
        terms += [(k, X[:, k]) for k in range(6, 64)]
        a, rms, mx = fit(d, terms)
        for k in range(6):
            f = a[13 * k:13 * k + 13]
            if np.abs(f).max() < 1e-3:
                continue
            unit = np.zeros(13)
            unit[6] = 1
            if np.abs(f - unit).max() < 1e-2:
                plain[k] = b
            else:
                parts[(k, b)] = f
        for j, k in enumerate(range(6, 64)):
            g = a[78 + j]
            if abs(g) > 1e-3:
                assert abs(g - 1) < 1e-3, (mode, b, k, g)
                plain[k] = b
        log('A mode %d band %2d: rms %.2g, hybrid parts in QMF %s, whole QMF bands %s' % (
            mode, b, rms, [k for (k, bb) in parts if bb == b], [k for k, bb in plain.items() if bb == b]))
    split = sorted({k for k, _ in parts})
    assert split == list(range(len(split))), split
    lay = []
    for k in split:
        fb = {b: f for (kk, b), f in parts.items() if kk == k}
        best = None
        for Q in (2, 4, 6, 8, 12, 16):
            H = {b: np.array([abs(np.sum(f * np.exp(-1j * (2 * m + 1) * np.pi / Q * DD))) for m in range(Q)])
                 for b, f in fb.items()}
            sets = {b: [m for m in range(Q) if h[m] > 0.5] for b, h in H.items()}
            cover = sorted(m for s in sets.values() for m in s)
            margin = min(min(abs(v - 0.5) for v in h) for h in H.values())
            ok = cover == list(range(Q)) and all(sets.values())
            if ok and (best is None or margin > best[0]):
                best = (margin, Q, sets)
        assert best is not None, k
        margin, Q, sets = best
        log('A mode %d QMF %d: Q = %d (margin %.2f), sub-bands by parameter band %s' % (mode, k, Q, margin, sets))
        for b in sorted(sets):
            lay.append((k, Q, tuple(sets[b]), b))
    return lay, plain


# ---------------------------------------------------------------- B. prototypes
def prototypes(cfg, mode, nb, lay, plain, seed0, nf=32):
    nlow = 1 + max(k for k, _, _, _ in lay)
    qs = sorted({Q for _, Q, _, _ in lay}, reverse=True)
    NU = 7 * len(qs)
    AtA = np.zeros((NU, NU))
    Aty = np.zeros(NU)
    yty = 0.0
    nrow = 0
    per = []
    bins = set(range(0, 32 * (nlow + 1)))
    for b in sorted({b for _, _, _, b in lay}):
        idx = [0] * nb
        idx[b] = 7
        p = os.path.join(OUT, 'B%d_%02d.aac' % (mode, b))
        write(p, [payload(iid=idx, iid_mode=mode)] * nf, seed=seed0 + b, bins=bins)
        y = decode(p)
        X, _ = dump(p)
        n = len(X)
        start, end = 2 * 2048, 64 * n - 640
        r = np.asarray(y[:64 * n, 0] - y[:64 * n, 1], float)[start:end]
        for k, bb in plain.items():
            if bb == b:
                r = r - (cl7 - cr7) * psmodel.synth_band(X[:, k], k)[start:end]
        cols = np.zeros((end - start, NU))
        for k, Q, S, bb in lay:
            if bb != b:
                continue
            E = mods(Q, S)
            qi = qs.index(Q)
            for j in range(7):
                seq = sum(E[6 + d] * shift(X[:, k], d) for d in {j, -j})
                cols[:, 7 * qi + j] += psmodel.synth_band(seq, k)[start:end]
        A = (cl7 - cr7) * cols
        nrm = np.sqrt((A ** 2).sum(0))
        used = nrm > 1e-6 * nrm.max()
        th = np.full(NU, np.nan)
        th[used], *_ = np.linalg.lstsq(A[:, used], r, rcond=None)
        res = r - A[:, used] @ th[used]
        per.append(th)
        A[:, ~used] = 0
        AtA += A.T @ A
        Aty += A.T @ r
        yty += r @ r
        nrow += len(r)
        log('B config %d band %2d: rms %.3g (signal of L-R %.3g)' % (cfg, b, np.sqrt(np.mean(res ** 2)), np.sqrt(np.mean(r ** 2))))
    obs = np.diag(AtA) > 1e-6 * np.diag(AtA).max()
    th = np.full(NU, np.nan)
    sub = AtA[np.ix_(obs, obs)]
    th[obs] = np.linalg.solve(sub, Aty[obs])
    s2 = (yty - 2 * th[obs] @ Aty[obs] + th[obs] @ sub @ th[obs]) / nrow
    se = np.full(NU, np.nan)
    se[obs] = np.sqrt(np.diag(np.linalg.inv(sub)) * s2)
    per = np.array(per)
    out = {}
    for qi, Q in enumerate(qs):
        v = th[7 * qi:7 * qi + 7]
        e = se[7 * qi:7 * qi + 7]
        pp = per[:, 7 * qi:7 * qi + 7]
        spread = [float(np.nanmax(pp[:, j]) - np.nanmin(pp[:, j])) if np.sum(~np.isnan(pp[:, j])) > 1 else None
                  for j in range(7)]
        out[str(Q)] = {
            'n6_12': [None if np.isnan(x) else float(x) for x in v],
            'stderr': [None if np.isnan(x) else float(x) for x in e],
            'per_stream_spread': spread,
            'nearest_f32_ulps': [None if np.isnan(x) else float((x - float(np.float32(x))) / float(np.spacing(np.float32(x))))
                                 for x in v],
        }
        log('B config %d g_%d(n), n = 6..12: %s' % (cfg, Q, ' '.join('null' if np.isnan(x) else '%+.10f' % x for x in v)))
        log('    stderr %s' % ' '.join('-' if np.isnan(x) else '%.1e' % x for x in e))
    log('B config %d joint residual rms %.3g' % (cfg, np.sqrt(s2)))
    return out


lays = {}
for cfg, mode, s0 in ((20, 1, 11000), (34, 2, 12000)):
    lay, plain = layout(mode, NB[mode], s0)
    lays[cfg] = (lay, plain)
    qb = {}
    for k in sorted(plain):
        qb.setdefault(plain[k], []).append(k)
    R['layout_%d' % cfg] = {
        'split': [{'qmf': k, 'Q': Q, 'subbands': list(S), 'par': b} for k, Q, S, b in lay],
        'qmf_par': {str(k): plain[k] for k in sorted(plain)},
    }
    same = sorted((k, Q, tuple(S), b) for k, Q, S, b in hm.LOW[cfg]) == sorted(lay) and \
        {k: b for k, b in plain.items()} == hm.qmf_par(cfg)
    log('A config %d layout equals hybmodel: %s' % (cfg, same))
    R['layout_%d' % cfg]['equals_hybmodel'] = bool(same)

for cfg, mode, s0 in ((20, 1, 13000), (34, 2, 14000)):
    lay, plain = lays[cfg]
    R['proto_%d' % cfg] = prototypes(cfg, mode, NB[mode], lay, plain, s0)

T1 = {8: [0.125, 0.11793710567217, 0.09885108575264, 0.07266113929591, 0.04546865930473,
          0.02270420949825, 0.00746082949812],
      4: [0.25, 0.21227807049160, 0.12542448210445, 0.04318924038756, 0, -0.00794862316203,
          -0.00305151927305]}
R['ts26405_table1_diff'] = {}
for Q in (8, 4):
    v = R['proto_20'][str(Q)]['n6_12']
    diff = [None if x is None else x - t for x, t in zip(v, T1[Q])]
    R['ts26405_table1_diff'][str(Q)] = diff
    log('TS 26.405 Table 1 (Q = %d) minus fitted 20-band prototype: %s' % (
        Q, ' '.join('null' if x is None else '%+.1e' % x for x in diff)))

# ---------------------------------------------------------------- C. configuration
PAT_F = [15, -14, 13, -12, 11, -10, 9, -8, 7, -6, 5, -4, 3, -2, 1, -1, 2, -3, 4, -5, 6, -7, 8, -9,
         10, -11, 12, -13, 14, -15]
PAT_C = [7, -6, 5, -4, 3, -2, 1, -1, 2, -3, 4, -5, 6, -7]


def cfg_groups(X, cfg):
    lay, plain = lays[cfg]
    g = {}
    for k, Q, S, b in lay:
        f = proto_filter(cfg, Q, S)
        g.setdefault(b, []).append((k, hm._fir(X[:, k], f)))
    for k, b in plain.items():
        g.setdefault(b, []).append((k, X[:, k]))
    return [g[b] for b in sorted(g)]


def proto_filter(cfg, Q, S):
    v = [0.0 if x is None else x for x in R['proto_%d' % cfg][str(Q)]['n6_12']]
    g = np.concatenate([np.array(v[::-1]), np.array(v[1:])])
    f = g * mods(Q, S)
    return f


table = {}
modes = [None, 0, 1, 2, 3, 4, 5]
for im in modes:
    for cm in modes:
        if im is None and cm is None:
            continue
        p = os.path.join(OUT, 'C_%s_%s.aac' % (im, cm))
        if im is not None:
            pat = PAT_F if im >= 3 else PAT_C
            pl = payload(iid=[pat[i % len(pat)] for i in range(NB[im])], iid_mode=im,
                         icc=None if cm is None else [0] * NB[cm], icc_mode=1 if cm is None else cm)
        else:
            pl = payload(icc=[1 + i % 7 for i in range(NB[cm])], icc_mode=cm)
        write(p, [pl] * 6, seed=15000 + 10 * (0 if im is None else im + 1) + (9 if cm is None else cm))
        y = decode(p)
        X, _ = dump(p)
        sig = y[:, 0] if im is not None else (y[:, 0] + y[:, 1]) / 2
        rr = {c: group_fit(sig, X, cfg_groups(X, c))[1] for c in (20, 34)}
        c = min(rr, key=rr.get)
        table['%s,%s' % (im, cm)] = c
        rule = hm.config(im, cm)
        log('C iid %s icc %s: config %d (rms 20 %.2g, 34 %.2g), rule %s' % (im, cm, c, rr[20], rr[34],
                                                                          'ok' if rule == c else 'DIFFERS'))
R['config'] = {'by_iid_icc_mode': table,
               'rule': '34 bands when an enabled IID or ICC mode is 2 or 5, else 20',
               'rule_holds': all(hm.config(*[None if s == 'None' else int(s) for s in key.split(',')]) == v
                                 for key, v in table.items())}

# ---------------------------------------------------------------- D. maps
rng = np.random.default_rng(16000)
RULES = {'trunc0': lambda a, b: int((a + b) / 2), 'floor': lambda a, b: (a + b) // 2,
         'round_away': lambda a, b: int(np.sign(a + b) * ((abs(a + b) + 1) // 2))}


def read_back(kind, nbc, other, cfg, idx):
    p = os.path.join(OUT, 'D_%s_%d_%d_%d.aac' % (kind, nbc, cfg, read_back.n))
    read_back.n += 1
    mode = {10: 0, 20: 1}[nbc]
    if kind == 'iid':
        mode += 3   # fine grid: 31 distinct values
        pl = payload(iid=idx, iid_mode=mode, icc=None if other is None else [0] * NB[other],
                     icc_mode=1 if other is None else other)
    else:
        pl = payload(icc=idx, icc_mode=mode, iid=None if other is None else [0] * NB[other],
                     iid_mode=1 if other is None else other)
    write(p, [pl] * 6, seed=16000 + read_back.n)
    y = decode(p)
    X, _ = dump(p)
    gr = cfg_groups(X, cfg)
    if kind == 'iid':
        GL, r1, _ = group_fit(y[:, 0], X, gr)
        GR, r2, _ = group_fit(y[:, 1], X, gr)
        db = 20 * np.log10(np.abs(GL) / np.abs(GR))
        full = [-g for g in IID_FINE[:0:-1]] + IID_FINE
        got = [int(np.argmin(np.abs(np.array(full) - v))) - 15 for v in db]
        err = max(min(abs(np.array(full) - v)) for v in db)
    else:
        G, r1, _ = group_fit((y[:, 0] + y[:, 1]) / 2, X, gr)
        got = [int(np.argmin(np.abs(np.array(ICC_COS) - v))) for v in G.real]
        err = max(min(abs(np.array(ICC_COS) - v)) for v in G.real)
    return got, err


read_back.n = 0


def expected_map(nbc, cfg):
    if (nbc, cfg) == (10, 20):
        return list(hm.MAP10_20)
    m = list(hm.MAP20_34)
    if nbc == 20:
        return m
    to10 = hm.MAP10_20
    return [to10[e] if isinstance(e, int) else (to10[e[0]] if to10[e[0]] == to10[e[1]] else (to10[e[0]], to10[e[1]]))
            for e in m]


R['maps'] = {}
for kind in ('iid', 'icc'):
    for nbc, cfg, other in ((10, 20, None), (10, 34, 2), (20, 34, 2)):
        while True:  # three patterns that give every coded band a distinct triple of values
            pats = [[int(v) for v in (rng.integers(-15, 16, nbc) if kind == 'iid' else rng.integers(0, 8, nbc))]
                    for _ in range(3)]
            if len(set(zip(*pats))) == nbc:
                break
        obs = []
        for idx in pats:
            got, err = read_back(kind, nbc, other, cfg, idx)
            obs.append((idx, got))
        src = []
        for j in range(cfg):
            singles = [a for a in range(nbc) if all(o[1][j] == o[0][a] for o in obs)]
            if len(singles) == 1:
                src.append(singles[0])
                continue
            pairs = [(a, a + 1, rn) for a in range(nbc - 1) for rn, f in RULES.items()
                     if all(o[1][j] == f(o[0][a], o[0][a + 1]) for o in obs)]
            src.append(pairs if pairs else ('?', singles))
        key = '%s_%d_to_%d' % (kind, nbc, cfg)
        R['maps'][key] = src
        log('D %s: %s (read-back error %.1g)' % (key, src, err))
        exp = expected_map(nbc, cfg)
        same = all((s1 == e1) if isinstance(e1, int) else
                   (isinstance(s1, list) and (e1[0], e1[1], 'trunc0') in s1) for s1, e1 in zip(src, exp))
        R['maps'][key + '_equals_hybmodel'] = bool(same)
        log('    equals hybmodel (pairs: mean truncated toward zero): %s' % same)

# ---------------------------------------------------------------- E. verification
vr = []
rng = np.random.default_rng(17000)
for i, (kind, mode, other) in enumerate([('iid', 1, None), ('iid', 2, None), ('iid', 3, 5), ('icc', 1, 2),
                                         ('icc', 0, None), ('iid', 4, 2)]):
    if kind == 'iid':
        lim = 15 if mode >= 3 else 7
        idx = [int(v) for v in rng.integers(-lim, lim + 1, NB[mode])]
        im, cm = mode, other
        pl = payload(iid=idx, iid_mode=mode, icc=None if other is None else [0] * NB[other],
                     icc_mode=1 if other is None else other)
    else:
        idx = [int(v) for v in rng.integers(0, 8, NB[mode])]
        im, cm = other, mode
        pl = payload(icc=idx, icc_mode=mode, iid=None if other is None else [0] * NB[other],
                     iid_mode=1 if other is None else other)
    p = os.path.join(OUT, 'E%02d.aac' % i)
    write(p, [pl] * 16, seed=17000 + i)
    y = decode(p)
    X, _ = dump(p)
    n = len(X)
    cfg = hm.config(im, cm)
    par = hm.to_config(idx, NB[mode], cfg, kind)
    if kind == 'iid':
        tg = [(y[:, 0], [gains(v, mode >= 3)[0] for v in par]), (y[:, 1], [gains(v, mode >= 3)[1] for v in par])]
    else:
        tg = [((y[:, 0] + y[:, 1]) / 2, [ICC_COS[v] for v in par])]
    s, e = 2 * 2048, 64 * n - 640 - 6 * 64
    for sig, gp in tg:
        H = hm.analysis(X, cfg) * np.array([gp[b] for b in hm.PAR[cfg]])
        r1 = sig[s:e] - psmodel.synthesis(hm.synthesis(H, cfg))[s:e]
        gr = cfg_groups(X, cfg)
        pred = psmodel.synthesis(X) + sum((gp[b] - 1) * sum(psmodel.synth_band(sq, k) for k, sq in gr[b]) for b in range(cfg))
        r2 = sig[s:e] - pred[s:e]
        rms = float(np.sqrt(np.mean(sig[s:e] ** 2)))
        vr.append({'case': '%s mode %d other %s' % (kind, mode, other), 'signal_rms': rms,
                   'hybmodel_rms': float(np.sqrt(np.mean(r1 ** 2))), 'recovered_rms': float(np.sqrt(np.mean(r2 ** 2)))})
        log('E %s: signal rms %.3g, residual hybmodel %.3g, recovered values %.3g' % (
            vr[-1]['case'], rms, vr[-1]['hybmodel_rms'], vr[-1]['recovered_rms']))
R['verification'] = vr
R['seconds'] = time.time() - T0
json.dump(R, open(os.path.join(OUT, 'hyb.json'), 'w'), indent=1)
log('wrote', os.path.join(OUT, 'hyb.json'))
