"""Verification of timemodel.py on fresh random streams (seeds not used by any fit).
Each stream is a random sequence of PS frames: frame_class 0 with 0/1/2/4 envelopes,
frame_class 1 with random borders (equal ones included), dt-coded envelopes, header-less
frames, headers without IID, frames without PS or without the SBR element, illegal
indices, short extensions, reserved modes, non-monotone borders, and 20 <-> 34 changes.
IID-only streams are predicted channel by channel; ICC-only streams through (L+R)/2.
Parameter bands over QMF 0..5 share one value (a uniform gain there, so the hybrid
filters are transparent); the other bands get independent random indices. A change of
configuration always comes with a header and df-coded envelopes (dt and num_env 0 across
a change of band count were not probed).

usage: verify_time.py OUTDIR [seed0 [count]]"""
import sys
from timelib import *
import timemodel as TM

# QMF band -> parameter band; QMF 0..5 belong to the shared low group.
PB20 = [0] * 6 + QMF_PAR20[6:]
PB34 = [0] * 6 + [17, 18, 19, 20, 21, 22, 22, 23, 23, 24, 24, 25, 25, 26, 26, 27, 27, 27, 28, 28, 28,
                  29, 29, 29, 30, 30, 30, 31, 31, 31, 31, 32, 32, 32, 32] + [33] * 23
assert len(PB34) == 64
LOW = {20: 11, 34: 17}            # bands below this index form the low group
CONV = None      # timemodel's full conversion tables (t25)


P_SWITCH = float(os.environ.get('P_SWITCH', '0.12'))
START_CFG = int(os.environ.get('START_CFG', '20'))


def make_stream(rng, kind, nframes=24):
    """Returns (spec for build2, model frames)."""
    spec, model = [], []
    active_hdr = False
    cfg = START_CFG
    coded_cfg = None               # band count of the last frame that coded values
    hdr_mode = None                # mode of the last header (None: IID/ICC disabled)
    name ='IID' if kind == 'iid' else 'ICC'
    for f in range(nframes):
        r = rng.random()
        if f > 0 and r < 0.08:
            spec.append(None)
            model.append({'ps': False})
            continue
        if f > 0 and r < 0.11:
            spec.append(('nosbr',))
            model.append({'ps': False})
            continue
        if r < 0.14:
            spec.append(S.header() + S.grid(0, 1))   # header without IID and ICC
            model.append({'ps': True, 'header': {'iid_mode': None, 'icc_mode': None}, 'nenv': 1,
                          'frame_class': 0, 'iid': None, 'icc': None, 'cfg': None})   # keeps (t27)
            active_hdr = True
            coded_cfg = None
            hdr_mode = None
            continue
        if r < 0.17:
            spec.append(S.header(iid=6) + S.grid(0, 1) + '0' * 40)
            model.append({'ps': True, 'error': 'header'})
            active_hdr = False
            continue
        u = rng.random()
        new_cfg = 54 - cfg if (f > 0 and u < P_SWITCH) else cfg
        switch = new_cfg != cfg
        cfg = new_cfg
        mode = {20: 1, 34: 2}[cfg] + (3 if (kind == 'iid' and rng.random() < 0.4) else 0)
        # a header-less frame keeps the last header's mode
        header = switch or not active_hdr or hdr_mode is None or rng.random() < 0.7
        if not header:
            mode = hdr_mode
        nb = NB[mode]
        lim = 15 if (kind == 'iid' and mode >= 3) else 7
        lo, hi = (-lim, lim) if kind == 'iid' else (0, 7)
        fine = kind == 'iid' and mode >= 3
        fc = int(rng.random() < 0.4)
        if fc == 0:
            nenv = int(rng.choice([0, 1, 2, 4], p=[0.1, 0.4, 0.3, 0.2]))
            bords = ()
        else:
            nenv = int(rng.integers(1, 5))
            bords = tuple(sorted(int(x) for x in rng.integers(0, 32, nenv)))
            if rng.random() < 0.5:
                bords = bords[:-1] + (31,)
            if rng.random() < 0.08 and nenv >= 2 and bords[0] != bords[-1]:
                bords = tuple(reversed(bords))
        same_bands = coded_cfg == nb
        if nenv == 0 and not same_bands:
            nenv = 1
        if header:
            hdr_mode = mode
        coded, bits = [], ''
        for e in range(nenv):
            low = int(rng.integers(lo, hi + 1))
            v = [low] * LOW[cfg] + [int(x) for x in rng.integers(lo, hi + 1, nb - LOW[cfg])]
            if e == 0 and rng.random() < 0.06:
                v[nb - 2], v[nb - 1] = hi, hi + 1                 # illegal
            if same_bands and rng.random() < 0.3:
                d = [int(rng.integers(-2, 3))] * LOW[cfg] + [int(x) for x in rng.integers(-2, 3, nb - LOW[cfg])]
                coded.append((True, d))
                bits += '1' + ''.join(B[name + ('_DT_FINE' if fine else '_DT')][x] for x in d)
            else:
                coded.append((False, list(np.diff([0] + v))))
                bits += '0' + df_codes(v, name + ('_DF_FINE' if fine else '_DF'))
        s = (S.header(iid=mode if kind == 'iid' else None, icc=mode if kind == 'icc' else None)
             if header else '0')
        idx = {0: 0, 1: 1, 2: 2, 4: 3}[nenv] if fc == 0 else nenv - 1
        s += S.grid(fc, idx, bords) + bits
        h = {'iid_mode': mode if kind == 'iid' else None, 'icc_mode': mode if kind == 'icc' else None}
        if header:
            active_hdr = True
        if rng.random() < 0.05:
            spec.append(('declared', s, 1))
            m = {'ps': True, 'error': 'length', 'nenv': nenv, 'frame_class': fc,
                 'borders': bords, 'header': h if header else None, 'cfg': cfg, 'iid': None, 'icc': None}
            m['iid' if kind == 'iid' else 'icc'] = coded
            model.append(m)
            active_hdr = False
            continue
        spec.append(s)
        m = {'ps': True, 'header': h if header else None, 'frame_class': fc, 'nenv': nenv,
             'borders': bords, 'cfg': cfg, 'iid': None, 'icc': None}
        m['iid' if kind == 'iid' else 'icc'] = coded
        model.append(m)
        coded_cfg = nb
    # two trailing frames without PS, so that every random frame is fully compared
    return spec + [None, None], model + [{'ps': False}, {'ps': False}]


def predict(X, outs, kind):
    n = len(X)
    G = np.ones((2, n, 64))
    for f, o in enumerate(outs):
        if not o.processed:
            continue
        pb = PB20 if o.cfg == 20 else PB34
        for c, col in ((0, 0), (1, 2)):
            G[c, 32 * f:32 * f + 32] = o.H[:, pb, col]
    if kind == 'iid':
        return [psmodel.synthesis(G[c] * X) for c in (0, 1)]
    return [psmodel.synthesis(0.5 * (G[0] + G[1]) * X)]


def describe(m):
    mm = dict(m)
    for key in ('iid', 'icc'):
        if mm.get(key):
            mm[key] = ''.join('t' if dt else 'f' for dt, _ in mm[key])
    return mm


def main(out, seed0, count):
  worst = 0.0
  for i in range(count):
    for kind in ('iid', 'icc'):
        seed = seed0 + i
        rng = np.random.default_rng(seed)
        spec, model = make_stream(rng, kind)
        p = os.path.join(out, 'v_%s_%d.aac' % (kind, seed))
        build2(p, spec, seed=seed)
        y, msgs, _ = decode_log(p)
        X, _ = dump(p)
        tl = TM.Timeline(conv=CONV)
        outs = [tl.frame(m) for m in model]
        nf = len(X) // 32
        outs = outs[:nf]
        pred = predict(X, outs, kind)
        sigs = [y[:, 0], y[:, 1]] if kind == 'iid' else [(y[:, 0] + y[:, 1]) / 2]
        for c, (s_ref, s_pred) in enumerate(zip(sigs, pred)):
            m = min(len(s_ref), len(s_pred)) - 640
            d = s_ref[:m] - s_pred[:m]
            # frames whose start reads parameter bands of the hybrid region with distinct
            # values (a read across the 20/34 layouts) need the hybrid model: not compared
            skip = set()
            for f, o in enumerate(outs):
                if o.processed and o.start is not None:
                    low = o.start[:LOW[o.cfg]]
                    if np.abs(low - low[0]).max() > 1e-12:
                        skip |= {f, f + 1}
            per = {f: float(np.sqrt(np.mean(d[2048 * f:2048 * f + 2048] ** 2)))
                   for f in range(m // 2048) if f not in skip}
            rms = float(np.sqrt(np.mean(np.square(list(per.values())))))
            worst = max(worst, rms)
            print('%s seed %d ch %d: residual rms %.2g over %d compared frames (signal rms %.2g); worst frame %.2g' % (
                kind, seed, c, rms, len(per), np.sqrt(np.mean(s_ref[:m] ** 2)), max(per.values())))
            if skip:
                print('   not compared (hybrid-region start not uniform):', sorted(skip))
            bad = [f for f, v in per.items() if v > 5e-8]
            if bad:
                print('   frames above 5e-8:', bad)
                for f in bad[:4]:
                    print('     f%d %s %s' % (f, outs[f].reason, describe(model[f])))
        print('   frame kinds:', ' '.join(o.reason[0] if o.reason else '?' for o in outs),
              ' msgs:', len(msgs))
  print('worst residual rms %.3g' % worst)
  return worst


if __name__ == '__main__':
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    if len(sys.argv) > 4:
        P_SWITCH = float(sys.argv[4])
    if len(sys.argv) > 5:
        START_CFG = int(sys.argv[5])
    main(out,int(sys.argv[2]) if len(sys.argv) > 2 else 1001, int(sys.argv[3]) if len(sys.argv) > 3 else 4)
