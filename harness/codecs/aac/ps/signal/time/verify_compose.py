"""Verification of the composed whole model (compose.py: hybmodel + decmodel + mixmodel +
timemodel) on fresh random streams with IID and ICC both coded, random indices in every
band, and the frame kinds of verify_time.py (frame_class 0/1, dt, num_env 0, header-less
frames, headers without IID/ICC, frames without PS or without the SBR element, illegal
values, short extensions, reserved modes, decreasing borders), IID/ICC modes 0..5 (10, 20
and 34 coded bands, both mixing procedures, coarse and fine), and 20 <-> 34 changes with
probability p_switch per frame. dt coding and num_env 0 are only used while the coded band
count of the parameter is unchanged (unless allow_dt_change). Both channels are compared
with the reference over the whole stream, frame 0 included.

usage: verify_compose.py OUTDIR [seed0 [count [p_switch [policy-json [allow_dt_change]]]]]"""
import json
import sys

from timelib import *
import compose as C

H0_CFG = None   # config of a header without IID and ICC: None lets the model keep the previous one (t27)
NOSBR = False   # frames without the SBR element: their QMF 31..35 differ in the dump (SBR)
CFG_MODES = {20: (0, 1, 3, 4), 34: (2, 5)}


def make_stream(rng, nframes=24, p_switch=0.0, start_cfg=20, allow_change=False):
    spec, model = [], []
    active = False
    cfg = start_cfg
    hdr = None                    # (iid_mode, icc_mode) of the last header read
    coded_nb = {'IID': None, 'ICC': None}
    for f in range(nframes):
        r = rng.random()
        if f > 0 and r < 0.07:
            spec.append(None)
            model.append({'ps': False})
            continue
        if f > 0 and r < 0.09 and NOSBR:
            spec.append(('nosbr',))
            model.append({'ps': False})
            continue
        if r < 0.12:
            spec.append(S.header() + S.grid(0, 1))
            model.append({'ps': True, 'header': {'iid_mode': None, 'icc_mode': None}, 'nenv': 1,
                          'frame_class': 0, 'iid': None, 'icc': None, 'cfg': H0_CFG})
            active, hdr = True, (None, None)
            coded_nb = {'IID': None, 'ICC': None}
            continue
        if r < 0.14:
            spec.append(S.header(iid=6) + S.grid(0, 1) + '0' * 40)
            model.append({'ps': True, 'error': 'header'})
            active = False
            continue
        if f > 0 and rng.random() < p_switch:
            cfg = 54 - cfg
        header = (not active) or hdr is None or None in hdr or rng.random() < 0.7 or \
            C.HM.config(*hdr) != cfg
        if header:
            if cfg == 34:
                im, cm = int(rng.choice([0, 1, 2, 3, 4, 5])), int(rng.choice([2, 5]))
                if rng.random() < 0.5:
                    im, cm = cm if cm == 2 else 5, int(rng.choice([0, 1, 2, 3, 4, 5]))
            else:
                im, cm = int(rng.choice(CFG_MODES[20])), int(rng.choice(CFG_MODES[20]))
            hdr = (im, cm)
        im, cm = hdr
        assert C.HM.config(im, cm) == cfg
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
        same = {'IID': coded_nb['IID'] == NB[im], 'ICC': coded_nb['ICC'] == NB[cm]}
        if nenv == 0 and not (allow_change or (same['IID'] and same['ICC'])):
            nenv = 1
        coded = {}
        bits = ''
        for name, mode in (('IID', im), ('ICC', cm)):
            nb = NB[mode]
            fine = name == 'IID' and mode >= 3
            lo, hi = ((-15, 15) if fine else (-7, 7)) if name == 'IID' else (0, 7)
            cl = []
            for e in range(nenv):
                # a random walk over the bands (short codewords: a frame must fit the
                # SBR fill element's 269 bytes)
                v = list(np.clip(int(rng.integers(lo, hi + 1)) + np.cumsum(rng.integers(-3, 4, nb)), lo, hi))
                v = [int(x) for x in v]
                if e == 0 and rng.random() < 0.04:
                    v[nb - 2], v[nb - 1] = hi, hi + 1
                book = name + ('_DT_FINE' if fine else '_DT')
                if (same[name] or allow_change) and rng.random() < 0.3:
                    d = [int(x) for x in rng.integers(-2, 3, nb)]
                    cl.append((True, d))
                    bits += '1' + ''.join(B[book][x] for x in d)
                else:
                    cl.append((False, list(np.diff([0] + v))))
                    bits += '0' + df_codes(v, name + ('_DF_FINE' if fine else '_DF'))
            coded[name] = cl
        # ps_data order: header, grid, all IID envelopes, then all ICC envelopes
        iid_bits = ''
        s = (S.header(iid=im, icc=cm) if header else '0')
        idx = {0: 0, 1: 1, 2: 2, 4: 3}[nenv] if fc == 0 else nenv - 1
        s += S.grid(fc, idx, bords) + bits
        h = {'iid_mode': im, 'icc_mode': cm}
        if header:
            active = True
        m = {'ps': True, 'header': h if header else None, 'frame_class': fc, 'nenv': nenv,
             'borders': bords, 'cfg': cfg, 'iid': coded['IID'], 'icc': coded['ICC']}
        if rng.random() < 0.05 and len(s) > 6:     # a size of one byte holds 6 PS bits
            spec.append(('declared', s, 1))
            model.append(dict(m, error='length'))
            active = False
            continue
        spec.append(s)
        model.append(m)
        coded_nb = {'IID': NB[im], 'ICC': NB[cm]}
    return spec + [None, None], model + [{'ps': False}, {'ps': False}]


def run(out, seed, p_switch=0.0, start_cfg=20, policy=None, allow_change=False, show=True):
    rng = np.random.default_rng(seed)
    spec, model = make_stream(rng, p_switch=p_switch, start_cfg=start_cfg, allow_change=allow_change)
    p = os.path.join(out, 'vc_%d.aac' % seed)
    build2(p, spec, seed=seed)
    y, msgs, _ = decode_log(p)
    X, _ = dump(p)
    n = len(X)
    comp = C.Composer(policy)
    L, R = comp.run(X, model)
    # not modelled: a processed frame in another configuration than the last processed
    # one without a registered change (the change was read while the PS was off): the
    # comparison stops there
    corner, last = None, None
    for f, o in enumerate(comp.outs):
        if o.processed:
            if last is not None and o.cfg != last and not o.changed and corner is None:
                corner = f
            last = o.cfg
    # dt envelopes and num_env 0 frames whose kept values have another band count
    events, prev = 0, {'iid': None, 'icc': None}
    for fr in model:
        if not fr.get('ps') or fr.get('error') == 'header':
            continue
        for k in ('iid', 'icc'):
            c = fr.get(k)
            if c is None:
                continue
            if fr['nenv'] == 0 and prev[k] is not None and fr.get('header') is not None:
                im = fr['header']['iid_mode' if k == 'iid' else 'icc_mode']
                events += im is not None and NB[im] != prev[k]
            for dt, d in c:
                events += bool(dt) and prev[k] is not None and len(d) != prev[k]
            if c:
                prev[k] = len(c[0][1])
    per = []
    worst = 0.0
    for ch, Y in ((0, L), (1, R)):
        ym = psmodel.synthesis(Y)
        m = min(len(ym), len(y)) - 640
        d = y[:m, ch] - ym[:m]
        nf = m // 2048 - 1 if corner is None else min(corner, m // 2048 - 1)
        pf = [float(np.sqrt(np.mean(d[2048 * f:2048 * f + 2048] ** 2))) for f in range(nf)]
        per.append(pf)
        worst = max(worst, float(np.sqrt(np.mean(d[:2048 * len(pf)] ** 2))))
    bad = [f for f in range(len(per[0])) if max(per[0][f], per[1][f]) > 5e-8]
    sig = float(np.sqrt(np.mean(y ** 2)))
    if show:
        kinds = ' '.join((o.reason[0] if o.reason else '?') + ('*' if o.processed and o.cfg == 34 else '')
                         for o in comp.outs)
        print('seed %d: %d band-count events; residual rms %.2g (signal %.2g) over %d frames%s; frames above 5e-8: %s' % (
            seed, events, worst, sig, len(per[0]), '' if corner is None else ' (stopped at f%d)' % corner, bad))
        print('   kinds (* 34 bands): %s' % kinds)
    return worst, bad, model, comp


if __name__ == '__main__':
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    seed0 = int(sys.argv[2]) if len(sys.argv) > 2 else 8001
    count = int(sys.argv[3]) if len(sys.argv) > 3 else 4
    psw = float(sys.argv[4]) if len(sys.argv) > 4 else 0.0
    policy = dict(kv.split('=') for kv in sys.argv[5].split(',')) if len(sys.argv) > 5 and sys.argv[5] != '-' else None
    allow = len(sys.argv) > 6 and sys.argv[6] == '1'
    w = 0.0
    for i in range(count):
        for sc in (20, 34):
            r, bad, _, _ = run(out, seed0 + 2 * i + (sc == 34), psw, sc, policy, allow)
            w = max(w, r)
    print('worst residual rms %.3g' % w)
