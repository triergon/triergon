"""o30 (case 3): a frame without its SBR element in a PS stream. Neutral PS (IID and ICC
index 0, so L = R = the PS input once the first frame's ramp is over) and the same stream
without PS (HE-AAC v1). The reference's L slots of QMF bands LO..HI are fitted around
the frame (every other band taken from this decoder's PS input dump, which the v1
comparison shows to be right there), and compared with the dump: per band and slot.

usage: o30_no_sbr.py OUTDIR [seed] [frame]"""
from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
seed = int(sys.argv[2]) if len(sys.argv) > 2 else 11
NOSBR = int(sys.argv[3]) if len(sys.argv) > 3 else 4
NF = 9
LO, HI = 26, 41
NEUTRAL = pdata(iid=[[0] * 20], icc=[[0] * 20])


def stream(path, ps):
    spec = []
    for f in range(NF):
        if f == NOSBR:
            spec.append(('nosbr',))
        else:
            spec.append(NEUTRAL if ps else None)
    build2(path, spec, seed=seed)


def fit_band_slots(y, X, f, bands):
    """Reference slots of `bands` over frame f (and 4 slots either side), the rest of the
    output being the synthesis of X."""
    s0, s1 = 32 * f - 4, 32 * f + 36
    known = X.copy()
    known[s0:s1, bands] = 0
    unk = []
    for t in range(s0, s1):
        for k in bands:
            V = np.zeros(64, complex)
            V[k] = 1
            unk.append([(t, V)])
    c, rms, mx, r = fit_rows(y, known, unk, 64 * (s0 - 12), 64 * (s1 + 12))
    return c.reshape(s1 - s0, len(bands)), rms, s0


if __name__ == '__main__':
    for ps in (True, False):
        p = os.path.join(out, 'o30_%s_%d.aac' % ('v2' if ps else 'v1', seed))
        stream(p, ps)
        y, msgs, _ = decode_log(p)
        z = rust(p)
        r = per_frame(z, y).max(1)
        print('== %s seed %d, frame %d without SBR: rust - ref per frame %s  msgs %s' % (
            'v2 neutral PS' if ps else 'v1', seed, NOSBR, fmt(r), msgs))
        if not ps:
            continue
        X, raw = dump(p)
        bands = list(range(LO, HI))
        Z, rms, s0 = fit_band_slots(y[:, 0], X, NOSBR, bands)
        print('   fit rms %.1e; per band rms |ref - dump| / |dump| over the frame:' % rms)
        a = 32 * NOSBR - s0
        for j, k in enumerate(bands):
            d = Z[a:a + 32, j] - X[32 * NOSBR:32 * NOSBR + 32, k]
            print('   QMF %2d: |dump| %.2e |ref| %.2e |diff| %.2e' % (
                k, np.sqrt(np.mean(np.abs(X[32 * NOSBR:32 * NOSBR + 32, k]) ** 2)),
                np.sqrt(np.mean(np.abs(Z[a:a + 32, j]) ** 2)), np.sqrt(np.mean(np.abs(d) ** 2))))
        for k in range(30, 37):
            j = bands.index(k)
            print('   QMF %d per slot (t: dump | ref):' % k)
            print('     ' + ' '.join('%d:%.0f|%.0f' % (t - a, abs(X[s0 + t, k]), abs(Z[t, j])) for t in range(len(Z))))
