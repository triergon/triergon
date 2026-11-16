"""o13 (case 1): the persistent-slot rule (olib.PersistComposer: the decorrelator's arrays
kept per slot in the reference's order, a layout reading and writing only its own slots,
the all-pass sections written by all-pass bands only, everything zeroed at a registered
change) against the reference on
  A  o01's streams (direct, err, ro, len; both directions)
  B  o09's silent-corner streams
  C  two corners in a row (34 -> 20 -> 34 and 20 -> 34 -> 20, by ro and len frames), ICC 7
     and random IID/ICC, so that slots one layout leaves alone come back stale
  D  the open stream (verify_compose seed 9412), verify_compose's corner seeds 8107, 8112,
     8316, and fresh random streams of verify_compose's generator with many 20 <-> 34
     changes and error frames
Whole streams from frame 0; frames above 5e-8 are listed. With DECODE_CHECK set to a
decode_aac binary, that decoder is compared with the reference on the same streams too.

usage: o13_verify_change_off.py OUTDIR [parts] [policy k:v,...]"""
import o01_change_off as O1
import o09_silent_corner as O9

from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
PARTS = sys.argv[2] if len(sys.argv) > 2 else 'ABCD'
POL = {'form': 'lat'}
if len(sys.argv) > 3:
    POL.update(dict(kv.split(':') for kv in sys.argv[3].split(',')))
FLOOR = 5e-8
worst = {}


def check(name, p, model, part):
    y, msgs, _ = decode_log(p)
    X, _ = dump(p)
    comp = PersistComposer(POL)
    L, R = comp.run(X, model)
    m = model_out(L, R, len(y))
    r = per_frame(m, y)[:-1].max(1)   # the last frame is cut by the synthesis tail
    bad = [f for f in range(len(r)) if r[f] > FLOOR]
    tot = float(np.sqrt(np.mean((m[:len(r) * 2048] - y[:len(r) * 2048]) ** 2)))
    worst[part] = max(worst.get(part, 0), tot)
    extra = ''
    if os.environ.get('DECODE_CHECK'):
        # a decoder build against the reference on the same stream
        z = rust(p, os.environ['DECODE_CHECK'])
        rz = per_frame(z, y)[:-1].max(1)
        tz = float(np.sqrt(np.mean((z[:len(rz) * 2048] - y[:len(rz) * 2048]) ** 2)))
        worst[part + ' decoder'] = max(worst.get(part + ' decoder', 0), tz)
        extra = ' | decoder rms %.2e above floor %s' % (tz, [f for f in range(len(rz)) if rz[f] > FLOOR])
    print('  %-34s rms %.2e  worst frame %.1e  corners %s  above floor %s%s' % (
        name, tot, r.max(), comp.corner, bad, extra))
    return comp


def rnd_payload(rng, cfg, header=True, modes=None):
    """A random valid frame of the layout cfg: (bits, frame dict)."""
    if header:
        im = int(rng.choice([1, 4] if cfg == 20 else [2, 5]))
        cm = int(rng.choice([1, 4] if cfg == 20 else [2, 5]))
    else:
        im, cm = modes
    nb = {1: 20, 4: 20, 2: 34, 5: 34}
    lim = 15 if im >= 3 else 7
    iid = list(np.clip(np.cumsum(rng.integers(-2, 3, nb[im])), -lim, lim))
    icc = list(np.clip(np.abs(np.cumsum(rng.integers(-1, 2, nb[cm]))), 0, 7))
    return pf(iid=[int(v) for v in iid], icc=[int(v) for v in icc], iid_mode=im, icc_mode=cm,
              header=header, modes=modes), (im, cm)


def two_corners(a, b, via, kind, rng):
    """a-layout frames, a corner to b (by `via`: ro or len), b frames, a corner back."""
    fr = []

    def ok(cfg, hdr=True, modes=None):
        if kind == 'icc7':
            nb = cfg
            return pf(icc=[7] * nb, icc_mode=O1.MODE[cfg]), (None, O1.MODE[cfg])
        return rnd_payload(rng, cfg, hdr, modes)

    def bad(cfg):
        s, d = pf(icc=[7, 8] + [8] * (cfg - 2), icc_mode=O1.MODE[cfg])
        return s, d

    def corner(cfg):
        if via == 'ro':
            # an error frame with an ICC-only header of cfg, then a header-less frame
            return [bad(cfg), pf(icc=[3] * cfg, header=False, modes=(None, O1.MODE[cfg]))]
        s, d = ok(cfg)[0]
        return [(('declared', s, 1), dict(d, error='length'))]

    for _ in range(3):
        fr.append(ok(a)[0])
    fr += corner(b)
    for _ in range(3):
        fr.append(ok(b)[0])
    fr += corner(a)
    for _ in range(3):
        fr.append(ok(a)[0])
    return fr + [nops(), nops()]


if __name__ == '__main__':
    if 'A' in PARTS:
        print('== A: o01 streams')
        for oc, nc in ((34, 20), (20, 34)):
            for kind in ('direct', 'err', 'ro', 'len'):
                p = os.path.join(out, 'A_%s_%d_%d.aac' % (kind, oc, nc))
                model = build_frames(p, O1.stream(kind, oc, nc), seed=11)
                check('%s %d->%d' % (kind, oc, nc), p, model, 'A')
    if 'B' in PARTS:
        print('== B: silent corner')
        O9.out = out
        for seed in (21, 22, 23):
            for oc, nc in ((34, 20), (20, 34)):
                p = os.path.join(out, 'B_%d_%d_%d.aac' % (oc, nc, seed))
                model = build_frames(p, O9.stream(oc, nc), seed=seed)
                check('silent %d->%d seed %d' % (oc, nc, seed), p, model, 'B')
    if 'C' in PARTS:
        print('== C: two corners in a row')
        for kind in ('icc7', 'rand'):
            for a, b in ((34, 20), (20, 34)):
                for via in ('ro', 'len'):
                    for seed in (41, 42):
                        rng = np.random.default_rng(seed)
                        p = os.path.join(out, 'C_%s_%d_%d_%s_%d.aac' % (kind, a, b, via, seed))
                        model = build_frames(p, two_corners(a, b, via, kind, rng), seed=seed)
                        check('%s %d->%d->%d via %s seed %d' % (kind, a, b, a, via, seed), p, model, 'C')
    if 'E' in PARTS:
        # a registered change to 20 bands, then a corner back to 34: slots 71..90 and the
        # ducker's bands 20..33 hold what the last 34-band frame left unless the
        # registered change cleared every slot
        print('== E: registered change, then a corner')
        for kind in ('icc7', 'rand'):
            for via in ('ro', 'len'):
                for seed in (51, 52):
                    rng = np.random.default_rng(seed)
                    fr = two_corners(34, 20, via, kind, rng)
                    # replace the first corner (to 20) by a registered change: drop its
                    # error / read-only frames
                    k = 3
                    while fr[k][1].get('error') or fr[k][1].get('header') is None or \
                            fr[k][1].get('icc') and max(np.cumsum([d for d in fr[k][1]['icc'][0][1]])) > 7:
                        fr.pop(k)
                    p = os.path.join(out, 'E_%s_%s_%d.aac' % (kind, via, seed))
                    model = build_frames(p, fr, seed=seed)
                    check('%s 34=>20->34 via %s seed %d' % (kind, via, seed), p, model, 'E')
    if 'D' in PARTS:
        print('== D: random streams')
        runs = [(9412, 0.15, 34, True), (8107, 0.15, 20, False), (8112, 0.15, 34, False), (8316, 0.1, 34, True)]
        runs += [(9700 + i, 0.3, (20, 34)[i % 2], True) for i in range(16)]
        for seed, psw, sc, allow in runs:
            VC.NOSBR = False
            rng = np.random.default_rng(seed)
            spec, model = VC.make_stream(rng, p_switch=psw, start_cfg=sc, allow_change=allow)
            p = os.path.join(out, 'D_%d.aac' % seed)
            build2(p, spec, seed=seed)
            check('vc seed %d p %.2f start %d' % (seed, psw, sc), p, model, 'D')
    print('== worst stream rms per part: %s' % ', '.join('%s %.2e' % kv for kv in sorted(worst.items())))
