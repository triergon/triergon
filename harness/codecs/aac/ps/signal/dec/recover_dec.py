"""recover_dec.py OUTDIR: rerun the recovery of the PS decorrelator and its ducking from
scratch against the pinned reference and write OUTDIR/dec.json.

Stages (each one crafts its streams, decodes them with the reference and fits; the probe of
the notes that each stage repeats is named):
 1. QMF all-pass chain (D06..D08): single-QMF parameter bands 4, 7, 8 of the 20-band
    configuration, steady tone plus weak noise, ICC 7 in the band. Unconstrained fit of
    G z^-d0 prod (alpha_m + z^-d_m) / (1 + beta_m z^-d_m), d = 3, 4, 5, from random starts;
    d0 chosen by residual. From the fits: the all-pass form (alpha = conj(beta)), a_m =
    |beta_m|, Q_m = -beta_m / |beta_m|; A_m and the decay slope by a linear fit of a_m over
    k; q_m from the phase step between bands 7 and 8; the frequency offset of Q_m and G.
 2. Region borders, plain delays and the decay start (D10..D12), on whole-band tone streams
    (ICC 7 everywhere) of both configurations: coordinate descent from a wrong start.
 3. Low hybrid bands (D16, D26, D27): per hybrid band of the hyb recovery, sub-bands apart or summed,
    and every sub-band frequency among the rule and its alternatives.
 4. Ducking (D20..D24): constants by a grid and Gauss-Newton on a tone-step stream; the
    detection grouping by residual on whole-band noise streams.
 5. Joint refinement (D25) of the linear constants (tones) and the ducking constants
    (noise), 20 bands; the 34-band residuals with the same constants.
 6. Verification (D28) on fresh streams of psgen's codebook-1 frames.
"""
import sys
import json
import time
from dl import *
import dgen
import sig
import apfit
import decmodel as dm

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
T0 = time.time()
J = {'stages': {}}


def note(stage, key, val):
    J['stages'].setdefault(stage, {})[key] = val
    print('[%6.1f s] %s %s: %s' % (time.time() - T0, stage, key, val))
    sys.stdout.flush()


def db(r, s):
    return float(20 * np.log10(r / s))


# ---------------------------------------------------------------- stage 1
rng = np.random.default_rng(20261113)
fits = {}
for k in (8, 7, 4):
    p = os.path.join(out, 's1_k%d.aac' % k)
    y, X, grp = band_stream(p, 20, k, NF=12)
    n = len(X)
    known = X.copy()
    known[:, grp] = 0
    R = slice(2 * 2048, 64 * n - 640)
    target = (y[:64 * n, 0] - psmodel.synthesis(known))[R]
    sD = rms(target)
    d0s = (1, 2, 3) if k == 8 else (fits[8]['d0'],)
    best = None
    for d0 in d0s:
        bd = None
        for trial in range(10 if k == 8 and d0 != 2 else 40):
            ph = rng.uniform(0, 2 * np.pi, 4)
            p0 = [np.cos(ph[3]), np.sin(ph[3])]
            for i in range(3):
                q = np.exp(1j * ph[i])
                # magnitudes: 0.4, or (every other trial) those of the band-8 fit
                a0 = fits[8]['a'][i] if (8 in fits and trial % 2 == 0) else 0.4
                p0 += [(-a0 * np.conj(q)).real, (-a0 * np.conj(q)).imag, (-a0 * q).real, (-a0 * q).imag]
            with np.errstate(all='ignore'):
                pp, r = apfit.gn(target, X[:, k], k, d0, (3, 4, 5), p0, R, iters=30)
            if not np.isfinite(r):
                continue
            if bd is None or r < bd[0]:
                bd = (r, d0, pp)
            if r < 1e-4 * sD:
                break
        if best is None or bd[0] < best[0]:
            best = bd
        if k == 8:
            note('1', 'k8 d0=%d best residual (dB)' % d0, db(bd[0], sD))
    r, d0, pp = best
    if db(r, sD) > -90:
        raise SystemExit('stage 1: band %d did not converge (%.1f dB)' % (k, db(r, sD)))
    G, secs = apfit.unpack(pp, (3, 4, 5))
    fits[k] = dict(d0=d0, G=G, a=[abs(b_) for _, _, b_ in secs], Q=[-b_ / abs(b_) for _, _, b_ in secs],
                   allpass_err=max(abs(a_ - np.conj(b_)) for _, a_, b_ in secs), db=db(r, sD))
    note('1', 'k%d fit' % k, dict(residual_db=fits[k]['db'], d0=d0, absG=abs(G), a=fits[k]['a'],
                                   max_abs_alpha_minus_conj_beta=fits[k]['allpass_err']))
PRE = fits[8]['d0']
ks = np.array([4, 7, 8])
A_, S_ = [], []
for m in range(3):
    am = np.array([fits[k]['a'][m] for k in ks])
    c1, c0 = np.polyfit(ks, am, 1)  # a = c0 + c1 k
    A3 = c0 + 3 * c1
    A_.append(A3)
    S_.append(-c1 / A3)
note('1', 'a_m(k) = A_m (1 - S (k - 3)): A_m', A_)
note('1', 'S per section', S_)
QF_, OFF = [], []
for m in range(3):
    dth = np.angle(fits[8]['Q'][m] / fits[7]['Q'][m])
    q = (-dth / np.pi) % 2
    QF_.append(q)
    v = -np.angle(fits[8]['Q'][m]) / np.pi  # = q (8 + c) mod 2
    c = (v / q - 8) % (2 / q)
    OFF.append(c)
# the unconstrained form has G = phi prod_m Q_m (each section is the all-pass divided by Q_m)
phi8 = fits[8]['G'] / np.prod(fits[8]['Q'])
phi7 = fits[7]['G'] / np.prod(fits[7]['Q'])
F0_ = (-np.angle(phi8 / phi7) / np.pi) % 2
cG = ((-np.angle(phi8) / np.pi) / F0_ - 8) % (2 / F0_)
note('1', 'q_m from the 7 -> 8 phase step', QF_)
note('1', 'frequency offset c of Q_m (phase = -pi q (k + c))', OFF)
note('1', 'q0 of phi = G / prod Q_m, and its offset', [F0_, cG])
C = dict(A=tuple(A_), QF=tuple(QF_), F0=F0_, SLOPE=float(np.mean(S_)), PRE=PRE)

# ---------------------------------------------------------------- stage 2/3/5 streams
NF = 20
streams = {}
for cfg in (20, 34):
    fl = [f for _, _, _, _, f in dm.LOW[cfg] if f > 0] + [k + 0.5 for k in range(dm.NLOW[cfg], 32)]
    tones = sum(sig.tone(NF, f * sig.BW, phase=1.3 * i) for i, f in enumerate(fl))
    sigs = {'tones': 30 * (tones + sig.band_noise(NF, 5, 31.5 * sig.BW, 0.01 * np.sqrt(len(fl)), seed=1)),
            'noise': 30 * sig.band_noise(NF, 5, 31.5 * sig.BW, np.sqrt(len(fl)) / np.sqrt(2), seed=2)}
    for name, s in sigs.items():
        p = os.path.join(out, '%s%d.aac' % (name, cfg))
        dgen.write_signal(p, s, payloads=[pslib.payload(icc=[7] * cfg, icc_mode={20: 1, 34: 2}[cfg])] * NF, env_level=0)
        y = decode(p)
        X, _ = dump(p)
        streams[(name, cfg)] = (y, X)


def evaluate(name, cfg, consts, low, detect='par', ch=0):
    """Residual of output channel ch against the model: D in the parameter bands with ICC
    7 (all of them unless the stream says otherwise), M elsewhere."""
    y, X = streams[(name, cfg)][:2]
    decset = streams[(name, cfg)][2] if len(streams[(name, cfg)]) > 2 else None
    n = len(X)
    H = dm.analysis(X, cfg, low)
    dec = dm.Decorrelator(cfg, dict(consts, LOW=low))
    D = dec.linear(H)
    bl = dm.bands(cfg, low)
    if detect == 'hybband':  # power of the hyb recovery's hybrid bands (sub-bands summed first)
        nb = 1 + max(b for _, b, _, _ in bl)
        P = np.zeros((n, nb))
        acc = {}
        for j, (k, b, f, kind) in enumerate(bl):
            acc[(k, b)] = acc.get((k, b), 0) + H[:, j]
        for (k, b), v in acc.items():
            P[:, b] += np.abs(v) ** 2
        G = dm.duck_gain(P, consts)
        D = D * G[:, [b for _, b, _, _ in bl]]
    elif detect != 'none':
        dec.c['DETECT'] = detect
        G = dec.gains(H)
        D = D * (G if detect == 'band' else G[:, [b for _, b, _, _ in bl]])
    R = slice(2 * 2048, 64 * n - 640)
    sg = 1 if ch == 0 else -1
    if decset is not None:
        dec_ = np.array([b in decset for _, b, _, _ in bl])[None, :]
        D = np.where(dec_, sg * D, H)
        sg = 1
    m = psmodel.synthesis(dm.to_qmf(D, cfg, low))
    r = sg * y[:64 * n, ch] - m
    return rms(r[R]), rms(m[R]), m[R]


# ---------------------------------------------------------------- stage 2
STRUCT = {'AP_END': [21, 22, 23, 24, 25], 'SHORT_START': [33, 34, 35, 36, 37], 'LONG': [12, 13, 14, 15, 16],
          'SHORT': [0, 1, 2], 'K0': None}
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'hyb'))
import hybmodel  # noqa: E402

# initial guess for the low bands: every sub-band apart, at the frequency rule
LOW0 = {cfg: [(k, Q, (m,), b, dm.sub_freq(k, Q, m)) for k, Q, S, b in hybmodel.LOW[cfg] for m in S] for cfg in (20, 34)}
found = {}
for cfg in (20, 34):
    cur = dict(C, AP_END=21, SHORT_START=33, LONG=12, SHORT=0, K0=dm.NLOW[cfg] - 1)
    for sweep in range(3):
        changed = False
        for key, vals in STRUCT.items():
            vals = vals or [dm.NLOW[cfg] - 1, dm.NLOW[cfg], dm.NLOW[cfg] + 1]
            res = [(evaluate('tones', cfg, dict(cur, **{key: v}), LOW0[cfg])[0], v) for v in vals]
            v = min(res)[1]
            if v != cur[key]:
                cur[key], changed = v, True
        if not changed:
            break
    r, s_, _ = evaluate('tones', cfg, cur, LOW0[cfg])
    found[cfg] = {k: cur[k] for k in STRUCT}
    note('2', 'cfg %d structure' % cfg, dict(found[cfg], residual_db=db(r, s_)))
assert found[20]['AP_END'] == found[34]['AP_END'] and found[20]['LONG'] == found[34]['LONG']
for key in ('AP_END', 'SHORT_START', 'LONG', 'SHORT'):
    C[key] = found[20][key]
K0 = {cfg: found[cfg]['K0'] for cfg in (20, 34)}
note('2', 'decay start K0 minus NLOW', {cfg: K0[cfg] - dm.NLOW[cfg] for cfg in (20, 34)})

# ---------------------------------------------------------------- stage 3
LOWF = {}
for cfg in (20, 34):
    cc = dict(C, K0=K0[cfg])
    hb = hybmodel.LOW[cfg]
    low = list(LOW0[cfg])
    for b in sorted(set(e[3] for e in hb)):
        # stream: ICC 7 in b only; tone at the in-band sub-band's rule frequency
        subs = [(k, Q, m) for k, Q, S, bb in hb if bb == b for m in S]
        inb = [dm.sub_freq(k, Q, m) for k, Q, m in subs if k <= dm.sub_freq(k, Q, m) < k + 1]
        ft = inb[0] if inb else abs(dm.sub_freq(*subs[0]))
        ks_ = sorted(set(k for k, _, _ in subs) | set(qgroup(cfg, b)))
        s = 30 * (sig.tone(16, ft * sig.BW) + sig.band_noise(16, max(5.0, (min(ks_) - 0.5) * sig.BW), (max(ks_) + 1.5) * sig.BW, 0.01, seed=80 + b))
        p = os.path.join(out, 's3_c%d_b%d.aac' % (cfg, b))
        dgen.write_signal(p, s, payloads=[icc_payload([b], nb=cfg)] * 16, env_level=0)
        streams[('p%d' % b, cfg)] = (decode(p), dump(p)[0], {b})
        for sweep in range(2):
            for k, Q, S, bb in hb:
                if bb != b:
                    continue
                idx = [i for i, e in enumerate(low) if e[0] == k and e[3] == b]
                cands = []
                # apart: each sub-band's frequency among the rule and alternatives
                base = [e for i, e in enumerate(low) if i not in idx]
                pos = idx[0]
                trial = base[:pos] + [(k, Q, (m,), b, dm.sub_freq(k, Q, m)) for m in S] + base[pos:]
                for i in range(pos, pos + len(S)):
                    f0 = dm.sub_freq(k, Q, trial[i][2][0])
                    opts = []
                    for f in (f0, -f0, f0 - 2, f0 + 2):
                        t2 = list(trial)
                        t2[i] = trial[i][:4] + (f,)
                        opts.append((evaluate('p%d' % b, cfg, cc, t2)[0], f))
                    trial[i] = trial[i][:4] + (min(opts)[1],)
                cands.append((evaluate('p%d' % b, cfg, cc, trial)[0], 'apart', trial))
                if len(S) > 1:
                    for m in S:
                        t2 = base[:pos] + [(k, Q, tuple(S), b, dm.sub_freq(k, Q, m))] + base[pos:]
                        cands.append((evaluate('p%d' % b, cfg, cc, t2)[0], 'summed, f of sub-band %d' % m, t2))
                cands.sort(key=lambda v: v[0])
                low = cands[0][2]
                if sweep == 1:
                    sD = evaluate('p%d' % b, cfg, cc, low)[1]
                    sel = [e for e in low if e[0] == k and e[3] == b]
                    note('3', 'cfg %d par %d QMF %d' % (cfg, b, k), dict(
                        choice=cands[0][1], bands=[[list(e[2]), e[4], 'rule %.6f' % dm.sub_freq(k, Q, e[2][0])] for e in sel],
                        residuals_db={c[1]: round(db(c[0], sD), 1) for c in cands}))
    LOWF[cfg] = low
    r, s_, _ = evaluate('tones', cfg, cc, low)
    rule_ok = all(abs(e[4] - dm.sub_freq(e[0], e[1], e[2][0])) < 1e-12 for e in low)
    note('3', 'cfg %d low layout' % cfg, dict(bands=len(low), tones_residual_db=db(r, s_), every_band_at_the_rule_of_its_first_sub_band=rule_ok))

# ---------------------------------------------------------------- stage 4
k = 8
NFs = 32
Ls = sig.length(NFs)
e = np.ones(Ls)
r4 = np.random.default_rng(12)
t = 1024 * 3
while t < 1024 * 28:
    seg = int(r4.integers(300, 3000))
    e[t:t + seg] = 10 ** r4.uniform(-2.5, 0.3)
    t += seg
s = 30 * (e * sig.tone(NFs, sig.qmf_centre(k)) + sig.band_noise(NFs, (k - 1) * sig.BW, (k + 2) * sig.BW, 0.003, seed=3))
p = os.path.join(out, 's4_steps.aac')
dgen.write_signal(p, s, payloads=[icc_payload([par20(k)])] * NFs, env_level=0)
y, X = decode(p), dump(p)[0]
n = len(X)
known = X.copy()
known[:, k] = 0
lin = dm.allpass(X[:, k], k + 0.5, 1 - C['SLOPE'] * (k - K0[20]), C)
P = np.abs(X[:, k]) ** 2
R = slice(2 * 2048, 64 * n - 640)
target = (y[:64 * n, 0] - psmodel.synthesis(known))[R]
sD = rms(target)
note('4', 'step stream: residual without ducking (dB)', db(rms(target - psmodel.synth_band(lin, k)[R]), sD))
grid = []
for al in np.arange(0.50, 0.981, 0.04):
    for be in np.arange(0.05, 0.951, 0.10):
        for ga in np.arange(1.0, 3.001, 0.2):
            g = dm.duck_gain(P, dict(ALPHA=al, BETA=be, GAMMA=ga))
            grid.append((rms(target - psmodel.synth_band(g * lin, k)[R]), [al, be, ga]))
grid.sort(key=lambda v: v[0])
note('4', 'grid best (alpha, beta, gamma)', grid[0][1])
model = lambda q: psmodel.synth_band(dm.duck_gain(P, dict(ALPHA=q[0], BETA=q[1], GAMMA=q[2])) * lin, k)[R]
qq, r, cov = apfit.gn_generic(target, model, grid[0][1], iters=30, h=1e-8)
note('4', 'Gauss-Newton (alpha, beta, gamma)', dict(values=list(qq), sd=list(np.sqrt(np.abs(np.diag(cov)))), residual_db=db(r, sD)))
C.update(ALPHA=qq[0], BETA=qq[1], GAMMA=qq[2])
for cfg in (20, 34):
    cc = dict(C, K0=K0[cfg])
    res = {}
    for det in (('par', 'band', 'hybband', 'none') if cfg == 20 else ('par', 'band', 'none')):
        r, s_, _ = evaluate('noise', cfg, cc, LOWF[cfg], detect=det)
        res[det] = round(db(r, s_), 1)
    note('4', 'cfg %d detection grouping residuals (dB)' % cfg, res)

# ---------------------------------------------------------------- stage 5
names = ['A0', 'A1', 'A2', 'QF0', 'QF1', 'QF2', 'F0', 'SLOPE']


def lin_consts(pp, base):
    return dict(base, A=tuple(pp[0:3]), QF=tuple(pp[3:6]), F0=pp[6], SLOPE=pp[7])


cc = dict(C, K0=K0[20])
y, X = streams[('tones', 20)]
tg = y[:64 * len(X), 0][slice(2 * 2048, 64 * len(X) - 640)]
p0 = list(C['A']) + list(C['QF']) + [C['F0'], C['SLOPE']]
pp, r, cov = apfit.gn_generic(tg, lambda v: evaluate('tones', 20, lin_consts(v, cc), LOWF[20])[2], p0, iters=6, h=1e-7)
sd = np.sqrt(np.abs(np.diag(cov)))
note('5', 'linear constants (20-band tones)', {nm: [float(v), float(e_)] for nm, v, e_ in zip(names, pp, sd)})
C = lin_consts(pp, C)
cc = dict(C, K0=K0[20])
y, X = streams[('noise', 20)]
tg = y[:64 * len(X), 0][slice(2 * 2048, 64 * len(X) - 640)]
qq, r, cov = apfit.gn_generic(tg, lambda q: evaluate('noise', 20, dict(cc, ALPHA=q[0], BETA=q[1], GAMMA=q[2]), LOWF[20])[2],
                              [C['ALPHA'], C['BETA'], C['GAMMA']], iters=6, h=1e-8)
sd2 = np.sqrt(np.abs(np.diag(cov)))
note('5', 'ducking constants (20-band noise)', {nm: [float(v), float(e_)] for nm, v, e_ in zip(('ALPHA', 'BETA', 'GAMMA'), qq, sd2)})
C.update(ALPHA=qq[0], BETA=qq[1], GAMMA=qq[2])
for cfg in (20, 34):
    for name in ('tones', 'noise'):
        for ch in (0, 1):
            r, s_, _ = evaluate(name, cfg, dict(C, K0=K0[cfg]), LOWF[cfg], ch=ch)
            note('5', 'cfg %d %s ch %d residual (dB)' % (cfg, name, ch), round(db(r, s_), 1))

# ---------------------------------------------------------------- stage 6
for cfg in (20, 34):
    p = os.path.join(out, 'verify%d.aac' % cfg)
    pslib.write(p, [pslib.payload(icc=[7] * cfg, icc_mode={20: 1, 34: 2}[cfg])] * 16, seed=77 + cfg)
    streams[('verify', cfg)] = (decode(p), dump(p)[0])
    for ch in (0, 1):
        r, s_, _ = evaluate('verify', cfg, dict(C, K0=K0[cfg]), LOWF[cfg], ch=ch)
        note('6', 'cfg %d fresh codebook-1 stream ch %d' % (cfg, ch), dict(residual_rms=r, signal_rms=s_, db=round(db(r, s_), 1)))

J['constants'] = {
    'A': list(C['A']), 'QF': list(C['QF']), 'F0': C['F0'], 'SLOPE': C['SLOPE'], 'DELAYS': [3, 4, 5],
    'PRE': C['PRE'], 'LONG': C['LONG'], 'SHORT': C['SHORT'], 'AP_END': C['AP_END'], 'SHORT_START': C['SHORT_START'],
    'K0': {str(c): K0[c] for c in (20, 34)}, 'ALPHA': C['ALPHA'], 'BETA': C['BETA'], 'GAMMA': C['GAMMA'],
}
J['low_bands'] = {str(c): [dict(qmf=e[0], Q=e[1], subbands=list(e[2]), par=e[3], f=e[4]) for e in LOWF[c]] for c in (20, 34)}
J['qmf_par'] = {str(c): {str(k): dm.qmf_par(c, k) for k in range(dm.NLOW[c], 64)} for c in (20, 34)}
J['seconds'] = time.time() - T0
json.dump(J, open(os.path.join(out, 'dec.json'), 'w'), indent=1, default=float)
print('wrote %s in %.0f s' % (os.path.join(out, 'dec.json'), time.time() - T0))
