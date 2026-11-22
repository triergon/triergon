"""o43: which earlier values a time-differential IID envelope reads above the bands of the
frame before it. Shape: a 4-envelope 20-band frame A with different values per envelope,
a 1-envelope 10-band frame B, then a 1-envelope 20-band frame C coded time-differentially.
Candidates for C's reference above band 10: entries 10..19 of A's first envelope (one
array per envelope index, envelope 0 reading the array of the previous frame's last
envelope, here B's envelope 0, which A's envelope 0 wrote), or of A's last envelope (one
array per parameter). Each is compared with the same values coded frequency-
differentially; the one that matches is the reference. A second shape repeats this with
B a 2-envelope frame (its last envelope is 1, which A's envelope 1 wrote).

usage: o43_dt_reference_per_envelope.py OUTDIR"""
from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
rng = np.random.default_rng(43)


def walk(n, lo, hi):
    return [int(v) for v in np.clip(int(rng.integers(lo, hi + 1)) + np.cumsum(rng.integers(-2, 3, n)), lo, hi)]


a = [walk(20, -7, 7) for _ in range(4)]
icc20 = walk(20, 0, 7)
b = [walk(10, -7, 7) for _ in range(2)]
icc10 = walk(10, 0, 7)
A = pdata(iid=a, iid_mode=1, icc=[icc20] * 4, icc_mode=4)


def ref(spec):
    p = os.path.join(out, 'o43.aac')
    build2(p, spec, seed=43)
    return decode_log(p)[0]


for nb in (1, 2):
    B = pdata(iid=b[:nb], iid_mode=0, icc=[icc10] * nb, icc_mode=0)
    target = walk(20, -5, 5)

    def C(prev):
        t = [int(np.clip(p + np.clip(v - p, -2, 2), -7, 7)) for v, p in zip(target, prev)]
        return t, pdata(iid=[t], iid_mode=1, icc=[icc20], icc_mode=4, iid_dt=(0,), prev_iid=prev)

    for label, upper in (('A envelope %d (per envelope index)' % (nb - 1), a[nb - 1][10:]),
                         ('A envelope 3 (per parameter)', a[3][10:])):
        prev = b[nb - 1] + upper
        t, c = C(prev)
        dt = ref([A, B, c, None, None])
        df = ref([A, B, pdata(iid=[t], iid_mode=1, icc=[icc20], icc_mode=4), None, None])
        print('B with %d envelope(s), dt against %-36s vs df: %s' % (nb, label, fmt(per_frame(dt, df)[:-1].max(1))))
