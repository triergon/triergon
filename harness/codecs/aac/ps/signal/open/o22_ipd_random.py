"""o22 (case 2): fresh random streams with IPD/OPD across 20 <-> 34 changes, decoded by the
reference and by the builds given; whole streams from frame 0, frames above 5e-8 listed.

Per frame: a layout change with probability P_SWITCH; IID enabled (any mode of the layout,
coarse or fine) or disabled; ICC enabled (any mode of the layout); 1, 2 or 4 envelopes;
the PS extension with IPD/OPD (random indices, as many bands as the last IID mode gives),
with the enable bit 0, or absent; frames without PS; header-less frames; error frames
(an illegal ICC value). Set R ('regular'): random ICC indices, errors keep the layout, so
every 20 <-> 34 change is registered. Set K ('corners'): ICC index 0 everywhere (the
decorrelated part does not enter) and changes also registered while off (an error frame
with a header of the other layout, then a header-less frame). Set KR: K's corners with
random ICC indices (case 1's decorrelator rule and case 2's rules together). The
environment variables NENVS, EXTS (comma lists of envelope counts, and of ipd, off and
none for the extension) and PSW (change probability) choose the frame kinds.

usage: o22_ipd_random.py OUTDIR SET seed0 count binary [binary ...]"""
import o20_ipd_change as O20

from olib import *

NB = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}
MODES = {20: (1, 4), 34: (2, 5)}


LOG = []
# frame kinds drawn (environment NENVS, EXTS: comma lists)
NENVS = [int(v) for v in os.environ.get('NENVS', '1,1,2,4').split(',')]
EXTS = os.environ.get('EXTS', 'ipd,ipd,ipd,ipd,ipd,ipd,off,none').split(',')


def make(rng, corners, nframes=24, p_switch=0.25, icc0=None):
    pays = []
    cfg = int(rng.choice([20, 34]))
    hdr = None                 # (iid_mode, icc_mode) of the last header
    iid_nb = 10                # band count of the last IID mode (IPD bands follow it)
    active = False
    hdr_ext = False
    for f in range(nframes):
        r = rng.random()
        if f > 0 and r < 0.08:
            pays.append(None)
            continue
        if f > 0 and rng.random() < p_switch:
            cfg = 54 - cfg
            if corners and rng.random() < 0.6:
                # a change registered while off: error frame with a header of cfg, then a
                # header-less frame read while off
                cm = int(rng.choice(MODES[cfg])) if cfg == 34 else int(rng.choice((1, 4)))
                pays.append(payload(iid=None, icc=[7, 8] + [8] * (NB[cm] - 2), icc_mode=cm,
                                    ext=ipdopd_ext([0] * {10: 5, 20: 11, 34: 17}[iid_nb], [0] * {10: 5, 20: 11, 34: 17}[iid_nb])))
                hdr, hdr_ext = (None, cm), True
                n = {10: 5, 20: 11, 34: 17}[iid_nb]
                pays.append(noheader(iid=None, icc=[0] * NB[cm], icc_mode=cm,
                                     ext=ipdopd_ext([int(v) for v in rng.integers(0, 8, n)],
                                                    [int(v) for v in rng.integers(0, 8, n)])))
                active = False
                continue
        header = (not active) or hdr is None or rng.random() < 0.6 or \
            C.HM.config(*hdr) != cfg
        if header:
            # IID disabled or a mode of the layout's band count, ICC a mode of it (the
            # stream's first header enables IID: the IPD band count is then known)
            im = None if (rng.random() < 0.3 and hdr is not None) else int(rng.choice(MODES[cfg]))
            cm = int(rng.choice(MODES[cfg]))
            hdr = (im, cm)
        im, cm = hdr
        assert C.HM.config(im, cm) == cfg, (im, cm, cfg)
        if im is not None:
            iid_nb = NB[im]
        nenv = int(rng.choice(NENVS))
        n = {10: 5, 20: 11, 34: 17}[iid_nb]
        iid = None
        if im is not None:
            lim = 15 if im >= 3 else 7
            iid = [[int(v) for v in np.clip(int(rng.integers(-lim, lim + 1)) + np.cumsum(rng.integers(-2, 3, NB[im])), -lim, lim)]
                   for _ in range(nenv)]
        if corners if icc0 is None else icc0:
            icc = [[0] * NB[cm] for _ in range(nenv)]
        else:
            icc = [[int(v) for v in np.clip(int(rng.integers(0, 8)) + np.cumsum(rng.integers(-1, 2, NB[cm])), 0, 7)]
                   for _ in range(nenv)]
        bad = rng.random() < 0.06
        if bad:
            icc[0] = [7, 8] + [8] * (NB[cm] - 2)
        k = str(rng.choice(EXTS))
        if k == 'ipd':
            ext = '00' + '1'
            for e in range(nenv):
                ext += '0' + ipd_df([int(v) for v in rng.integers(0, 8, n)], 'IPD_DF') + \
                       '0' + ipd_df([int(v) for v in rng.integers(0, 8, n)], 'OPD_DF')
            ext += '0'
        elif k == 'off':
            ext = '00' + '0' + '0'
        else:
            ext = None
        kw = dict(iid=iid, iid_mode=im if im is not None else 1, icc=icc, icc_mode=cm, nenv=nenv)
        while len(payload(ext=ext, **kw)) > 1900 and kw['nenv'] > 1:
            # the fill element holds at most 269 bytes: fewer envelopes
            n2 = kw['nenv'] // 2
            kw = dict(kw, nenv=n2, iid=None if iid is None else iid[:n2], icc=icc[:n2])
            if ext is not None and ext[2] == '1':
                ext = '00' + '1' + ''.join('0' + ipd_df([int(v) for v in rng.integers(0, 8, n)], 'IPD_DF') +
                                           '0' + ipd_df([int(v) for v in rng.integers(0, 8, n)], 'OPD_DF')
                                           for _ in range(n2)) + '0'
        nenv = kw['nenv']
        LOG.append('f%d cfg %d hdr %s modes %s nenv %d ext %s bad %s' % (
            len(pays), cfg, header, (im, cm), nenv, 'ipd' if ext and ext[2] == '1' else ('off' if ext else None), bad))
        if header:
            pays.append(payload(ext=ext, **kw))
            hdr_ext = ext is not None
        else:
            # a header-less frame carries the extension exactly when the last header
            # enabled it
            if hdr_ext and ext is None:
                ext = '00' + '0' + '0'
            elif not hdr_ext:
                ext = None
            pays.append(noheader(ext=ext, **kw))
        active = not bad
    return pays + [None, None]


if __name__ == '__main__':
    out, which, seed0, count = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    bins = sys.argv[5:]
    os.makedirs(out, exist_ok=True)
    worst = [0.0] * len(bins)
    for seed in range(seed0, seed0 + count):
        rng = np.random.default_rng(seed)
        pays = make(rng, which in ('K', 'KR'), p_switch=float(os.environ.get('PSW', '0.25')),
                    icc0=which == 'K')
        p = os.path.join(out, 'o22_%s_%d.aac' % (which, seed))
        write(p, pays, seed=seed)
        y, msgs, _ = decode_log(p)
        line = []
        for i, b in enumerate(bins):
            z = rust(p, b)
            r = per_frame(z, y)[:-1].max(1)
            tot = float(np.sqrt(np.mean((z[:len(r) * 2048] - y[:len(r) * 2048]) ** 2)))
            worst[i] = max(worst[i], tot)
            line.append('rms %.2e above floor %s' % (tot, [f for f in range(len(r)) if r[f] > 5e-8]))
        other = sorted(set(m for m in msgs if m not in ('illegal icc',)))
        print('%s seed %d (%d msgs%s): %s' % (which, seed, len(msgs), ', other: %s' % other if other else '',
                                              ' | '.join(line)))
    print('worst stream rms per build: %s' % ', '.join('%.2e' % w for w in worst))
