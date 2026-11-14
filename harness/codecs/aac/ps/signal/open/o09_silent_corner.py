"""o09 (case 1): o01's ro streams with a silent core from the error frame on, so that the
first frame that runs after the change registered while off has no new input (beyond
the 6 history slots the hybrid analysis reads): its output is the decorrelator's carried
state alone. Several seeds; the composed model with the carry of o05 (lattice sections,
per-band arrays by band index, the ducker by parameter-band index) is compared per frame
and per QMF band.

usage: o09_silent_corner.py OUTDIR [seeds]"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
SEEDS = [21, 22, 23]
if __name__ == '__main__' and len(sys.argv) > 2:
    SEEDS = [int(s) for s in sys.argv[2].split(',')]
POL = {'form': 'lat', 'carry': 'index', 'duck': 'index', 'zero': 'oldplain'}


def stream(old, new, silent_from=4, kind='ro'):
    fr = O1.stream(kind, old, new)
    spec = []
    for i, (s, d) in enumerate(fr):
        if i >= silent_from:
            if s is None:
                s = {'extensions': [], 'silent': True}
            elif isinstance(s, tuple):
                s = {'extensions': [(2, s[1])], 'declared': s[2], 'silent': True}
            else:
                s = {'extensions': [(2, s)], 'silent': True}
        spec.append((s, d))
    return spec


def look(y, L, f):
    d = y[:, 0] - psmodel.synthesis(L)[:len(y)]
    Dq = psana.qmf_bands(np.concatenate([np.zeros(640), d]))
    s = slice(32 * f + 10, 32 * f + 42)
    return np.sqrt((np.abs(Dq[s]) ** 2).mean(0)) / 32768


def run(old, new, seed, pol=POL, kind='ro', show=True):
    p = os.path.join(out, 'o09_%s_%d_%d_%d.aac' % (kind, old, new, seed))
    model = build_frames(p, stream(old, new, kind=kind), seed=seed)
    y, msgs, _ = decode_log(p)
    X, _ = dump(p)
    comp = CarryComposer(pol)
    L, R = comp.run(X, model)
    r = per_frame(model_out(L, R, len(y)), y).max(1)
    corner = 6 if kind == 'ro' else 5
    if show:
        sig = np.sqrt(np.mean(np.abs(X[32 * corner:32 * corner + 32]) ** 2, 0)) / 32768
        print('== %s %d -> %d seed %d: frames %s | |X| corner QMF 0..5 %s' % (
            kind, old, new, seed, fmt(r[corner - 1:corner + 4]), fmt(sig[:6])))
        eb = look(y, L, corner)
        print('   corner QMF 0..15: %s' % fmt(eb[:16]))
    return p, model, y, X, comp, L


if __name__ == '__main__':
    pols = [('carry', POL)]
    if len(sys.argv) > 3:
        pols = [(a, dict(POL, **dict(kv.split(':') for kv in a.split(',')))) for a in sys.argv[3:]]
    for seed in SEEDS:
        for old, new in ((34, 20), (20, 34)):
            for name, pol in pols:
                print('-- policy', name)
                run(old, new, seed, pol)
