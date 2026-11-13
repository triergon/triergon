"""M26: per-frame gain ramps of every low hybrid band under OPD, with the hyb recovery's hybrid
filters (probes/hyb/hybmodel.py). For a configuration (20 or 34) and a set of parameter
bands holding OPD index 2 (IPD 0) from frame 0, 12 frames, seed 41, each low hybrid band
of the configuration (one per (QMF band, parameter band), mirror pairs as one band, and
optionally split into its single sub-bands) gets a free straight-line gain per frame;
QMF bands above are neutral. Printed: the residual, and the frame-6/7 start and end of
every hybrid band whose gain is not 1 (phase in units of pi/4).

usage: m26_hyb_ramps.py OUT CASE...   CASE = cfg:split:bands, e.g. 20:pairs:4,5"""
import sys
from mixlib import *

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'hyb'))
import hybmodel as HM  # noqa: E402

out = sys.argv[1]
NF = 12


def low_bands(cfg, split):
    """[(k, taps, par, label)] of the low hybrid bands; split: 'pairs' or 'single'."""
    res = []
    for k, Q, S, b in HM.LOW[cfg]:
        if split == 'single' and len(S) > 1:
            for m in S:
                res.append((k, HM.filt(Q, (m,), cfg), b, '%d:%d' % (k, m)))
        else:
            res.append((k, HM.filt(Q, S, cfg), b, '%d:%s' % (k, ','.join(map(str, S)))))
    return res


if __name__ == '__main__':
    for case in sys.argv[2:]:
        cfg, split, bl = case.split(':')
        cfg = int(cfg)
        bands = [int(v) for v in bl.split(',')] if bl else []
        mode = 1 if cfg == 20 else 2
        nb = 11 if cfg == 20 else 17
        opd = [2 if b in bands else 0 for b in range(nb)]
        pay = payload(iid=[0] * NB[mode], icc=[0] * NB[mode], iid_mode=mode, icc_mode=mode,
                      ext=ipdopd_ext([0] * nb, opd))
        p = os.path.join(out, 'c.aac')
        write(p, [pay] * NF, seed=41)
        y = decode(p)
        X, _ = dump(p)
        n = min(len(X), len(y) // 64)
        X = X[:n]
        LB = low_bands(cfg, split)
        seqs = [HM._fir(X[:, k], f) for k, f, b, lab in LB]
        j = (np.arange(n) % 32 + 1) / 32.0
        fr = np.arange(n) // 32
        print('=== %d bands, %s, OPD 2 in parameter bands %s' % (cfg, split, bands))
        for ch in (0, 1):
            T = Target(y[:, ch], zero_bands(X, range(HM.NLOW[cfg])))
            terms = []
            for (k, f, b, lab), s in zip(LB, seqs):
                for m in range(2, NF):
                    ind = fr == m
                    terms += [(k, s * ind * (1 - j)), (k, s * ind * j)]
            a, rms, mx, sr = T.fit(terms, 2 * 32 + 12, NF * 32 - 8)
            print(' ch %s rms %.3g of %.3g' % ('LR'[ch], rms, sr))
            if ch:
                continue
            for i, (k, f, b, lab) in enumerate(LB):
                base = i * 2 * (NF - 2)
                v = a[base + 8:base + 12]
                if np.abs(v - 1).max() > 1e-3:
                    print('   QMF %-8s par %2d: f6 %s' % (lab, b, ' '.join('%.4f@%+.4f' % (abs(x), np.angle(x) * 4 / np.pi) for x in v)))
