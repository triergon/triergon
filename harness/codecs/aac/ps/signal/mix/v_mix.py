"""V_MIX: verification of mixmodel.Mixer on fresh streams (seeds and patterns no fit used).
20-band configuration, 24 frames, 1, 2 or 4 envelopes per frame (frame_class 0), random
IID indices in all 20 bands, random ICC indices in parameter bands 18, 19 (QMF 23..63, where
D is a 14- or 1-slot delay of M, m01) and index 0 elsewhere (no D there), random IPD/OPD in
all 11 bands, with some frames without the extension and some with the IPD/OPD enable bit
0. Stream A: IID mode 1 / ICC mode 1 (first procedure, coarse IID). Stream B: IID mode 4 /
ICC mode 4 (second procedure, fine IID). 34-band configuration (ICC only in the top two ICC
bands, QMF >= 37, D a 1-slot delay there): C34 IID mode 2 / ICC mode 2, D34 IID mode 4 /
ICC mode 5 (20 -> 34 maps, second procedure, fine), E34 IID mode 0 / ICC mode 2 (10 -> 34).
C34 and E34 are also run without ICC, and with the dec recovery's D (decmodel, ducking included):
their cores have frames where D above QMF 37 is not a plain delay (m32: the ducker). The model output (the hyb recovery's filterbank,
mixmodel's matrices, phases and interpolation) is compared sample by sample with the
reference from frame 2 on."""
import sys
from mixlib import *
import mixmodel as MM

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'hyb'))
import hybmodel as HM  # noqa: E402

out = sys.argv[1] if __name__ == '__main__' else None
NF = 24


def ext_env(ipds, opds):
    s = '00' + '1'
    for i, o in zip(ipds, opds):
        s += '0' + ipd_df(i, 'IPD_DF') + '0' + ipd_df(o, 'OPD_DF')
    return s + '0'


def make(seed, iid_mode, icc_mode, envs=(1, 2, 4), use_icc=True, kinds=('ipd', 'ipd', 'ipd', 'none', 'off')):
    rng = np.random.default_rng(seed)
    fine = iid_mode >= 3
    nmax = 15 if fine else 7
    pays, frames = [], []
    for f in range(NF):
        nenv = int(rng.choice(list(envs)))
        ni, nc = NB[iid_mode], NB[icc_mode]
        nip = MM.NIPD[ni]
        iid = [[int(v) for v in rng.integers(-nmax, nmax + 1, ni)] for _ in range(nenv)]
        icc = [[0] * (nc - 2) + ([int(v) for v in rng.integers(0, 8, 2)] if use_icc else [0, 0]) for _ in range(nenv)]
        kind = str(rng.choice(list(kinds))) if f > 3 else kinds[0]
        ipd = [[int(v) for v in rng.integers(0, 8, nip)] for _ in range(nenv)]
        opd = [[int(v) for v in rng.integers(0, 8, nip)] for _ in range(nenv)]
        if kind == 'ipd':
            ext = ext_env(ipd, opd)
        elif kind == 'off':
            ext = '00' + '0' + '0'
        else:
            ext = None
        pays.append(payload(iid=iid, icc=icc, iid_mode=iid_mode, icc_mode=icc_mode, nenv=nenv, ext=ext))
        frames.append({'iid': iid, 'icc': icc, 'iid_mode': iid_mode, 'icc_mode': icc_mode, 'nenv': nenv,
                       'ipd': ipd, 'opd': opd, 'ext': {'ipd': 'ipd', 'off': 'off', 'none': None}[kind]})
    return pays, frames


def check(name, seed, iid_mode, icc_mode, cfg=20, outdir=None, mixer_kw=None, quiet=False, dec=False, **kw):
    """Model against reference; returns (rms L, rms R, max, signal rms)."""
    pays, frames = make(seed, iid_mode, icc_mode, **kw)
    p = os.path.join(outdir or out, 'v.aac')
    write(p, pays, seed=seed)
    y = decode(p)
    X, _ = dump(p)
    n = min(len(X), len(y) // 64, 32 * NF)
    X = X[:n]
    if dec:     # the dec recovery's D (ducking included) in its own band layout
        sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'dec'))
        import decmodel as DM
        mx = MM.Mixer(cfg, HM, DM=DM, **(mixer_kw or {}))
        D = DM.Decorrelator(cfg).process(DM.analysis(X, cfg))
    else:       # D as plain delays above QMF 23 (m01); ICC must be 0 below
        mx = MM.Mixer(cfg, HM, **(mixer_kw or {}))
        D = np.zeros((n, len(mx.lay)), complex)
        for j, (k, f, b, im) in enumerate(mx.lay):
            if f is None and 23 <= k <= 34:
                D[:, j] = shift(X[:, k], 14)
            elif f is None and k >= 35:
                D[:, j] = shift(X[:, k], 1)
    L, R = mx.run(X, D, frames)
    if not quiet:
        print('%s plan: %s' % (name, ' '.join('%d:%d%s' % (i, f['nenv'], {'ipd': 'P', 'off': 'o', None: '-'}[f['ext']])
                                           for i, f in enumerate(frames))))
    s0, s1 = 2 * 2048, 64 * n - 640
    res = []
    for ch, Y in ((0, L), (1, R)):
        ym = psmodel.synthesis(Y)
        r = y[s0:s1, ch] - ym[s0:s1]
        rms, mx_, sr = np.sqrt(np.mean(r ** 2)), np.abs(r).max(), np.sqrt(np.mean(y[s0:s1, ch] ** 2))
        res.append((float(rms), float(mx_), float(sr)))
        print('%s ch %s: residual rms %.3g max %.3g, signal rms %.3g (%.1f dB)' % (
            name, 'LR'[ch], rms, mx_, sr, 20 * np.log10(rms / sr)))
        if not quiet:
            e = [np.sqrt(np.mean(r[i:i + 2048] ** 2)) for i in range(0, len(r) - 2047, 2048)]
            print('   per frame (from 2):', ' '.join('%.1e' % v for v in e))
    return res


if __name__ == '__main__':
    if len(sys.argv) > 2 and sys.argv[2] == 'debug':
        check('C34 no icc', 79, 2, 2, cfg=34, use_icc=False)
        check('E34 no icc', 81, 0, 2, cfg=34, use_icc=False)
    else:
        check('A', 77, 1, 1)
        check('B', 78, 4, 4)
        check('C34', 79, 2, 2, cfg=34)
        check('D34', 80, 4, 5, cfg=34)
        check('E34', 81, 0, 2, cfg=34)
        check('C34 without ICC', 79, 2, 2, cfg=34, use_icc=False)
        check('E34 without ICC', 81, 0, 2, cfg=34, use_icc=False)
        check('C34 with dec D', 79, 2, 2, cfg=34, dec=True)
        check('E34 with dec D', 81, 0, 2, cfg=34, dec=True)
