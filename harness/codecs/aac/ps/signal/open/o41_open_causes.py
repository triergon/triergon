"""o41: what the differences of o40's noext-modes and vc-9818 depend on. Hand-built
streams of the same shape and variants that remove one ingredient each; this decoder
against the reference, per frame (frames above 1e-6 marked).

noext-modes shape: a header frame (IID mode 5, ICC mode 5) with IPD/OPD, an error frame,
then a header frame (IID mode 2, ICC mode 5) whose header disables the PS extension.
vc-9818 shape: a 20-band coarse IID frame, a 10-band IID frame (mode 0), then a 20-band
fine IID frame (mode 4) coded time-differentially.

usage: o41_open_causes.py OUTDIR [decode_aac]"""
from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
BIN = sys.argv[2] if len(sys.argv) > 2 else DECODE
rng = np.random.default_rng(41)


def run(name, spec, seed=41):
    p = os.path.join(out, 'o41_%s.aac' % name)
    build2(p, spec, seed=seed)
    y, msgs, _ = decode_log(p)
    z = rust(p, BIN)
    r = per_frame(z, y)[:-1].max(1)
    print('  %-34s %s %s  %s' % (name, fmt(r), '<' if r.max() > 1e-6 else ' ', sorted(set(msgs))))


def walk(n, lo, hi):
    return [int(v) for v in np.clip(int(rng.integers(lo, hi + 1)) + np.cumsum(rng.integers(-2, 3, n)), lo, hi)]


IPD17 = ipdopd_ext([int(v) for v in rng.integers(0, 8, 17)], [int(v) for v in rng.integers(0, 8, 17)])
IPD17b = ipdopd_ext([int(v) for v in rng.integers(0, 8, 17)], [int(v) for v in rng.integers(0, 8, 17)])
OFF = '00' + '0' + '0'
iid34a, iid34b = walk(34, -15, 15), walk(34, -7, 7)
icc34a, icc34b = walk(34, 0, 7), walk(34, 0, 7)


def f_a(ext=IPD17):
    return payload(iid=iid34a, iid_mode=5, icc=icc34a, icc_mode=5, ext=ext)


def f_err(header=True):
    bad = [7, 8] + [8] * 32
    if header:
        return payload(iid=iid34a, iid_mode=5, icc=bad, icc_mode=5, ext=IPD17)
    return noheader(iid=iid34a, iid_mode=5, icc=bad, icc_mode=5, ext=IPD17)


def f_c(ext=None):
    return payload(iid=iid34b, iid_mode=2, icc=icc34b, icc_mode=5, ext=ext)


print('== noext-modes')
run('a_err_noext', [f_a(), f_err(), f_c(), None, None])
run('a_err_ext_off', [f_a(), f_err(), f_c(OFF), None, None])
run('a_err_ext_ipd', [f_a(), f_err(), f_c(IPD17b), None, None])
run('a_noerr_noext', [f_a(), f_c(), None, None])
run('a_err_hdrless_noext', [f_a(), f_err(False), f_c(), None, None])
run('a_first_off_err_noext', [f_a(OFF), f_err(), f_c(), None, None])
run('a_first_noext_err_noext', [f_a(None), f_err(), f_c(), None, None])
run('a_err_noext_same_modes', [f_a(), f_err(), payload(iid=iid34b, iid_mode=5, icc=icc34b, icc_mode=5), None, None])

# what the reference makes of the header without the extension after the error: the same
# as an extension with the enable bit 0, or as IPD/OPD indices 0 with the bit 1?
def ref_out(spec, seed=41):
    p = os.path.join(out, 'o41_r.aac')
    build2(p, spec, seed=seed)
    return decode_log(p)[0]


ya = ref_out([f_a(), f_err(), f_c(), None, None])
for label, e in (('enable bit 0', OFF), ('indices 0', ipdopd_ext([0] * 17, [0] * 17)), ('the first frame\'s values', IPD17)):
    print('  reference, no extension vs %-24s %s' % (label + ':', fmt(per_frame(ya, ref_out([f_a(), f_err(), f_c(e), None, None]))[:-1].max(1))))

print('== vc-9818 shape')
v20 = walk(20, -7, 7)
v10 = walk(10, -7, 7)
icc20, icc10 = walk(20, 0, 7), walk(10, 0, 7)
# the kept values a 20-band dt envelope reads after a 10-band frame (time.md, t29): the
# 10-band frame's 10 values, then entries 10..19 of the last 20-band frame
kept = v10 + v20[10:]
t20 = walk(20, -7, 7)
t20 = [int(np.clip(k + np.clip(t - k, -2, 2), -7, 7)) for t, k in zip(t20, kept)]   # valid coarse and fine
A0 = pdata(iid=[v20], iid_mode=1, icc=[icc20], icc_mode=4)
A1 = pdata(iid=[v10], iid_mode=0, icc=[icc10], icc_mode=0)


def A2(mode=4, dt=True, prev=kept):
    return pdata(iid=[t20], iid_mode=mode, icc=[icc20], icc_mode=1, iid_dt=(0,) if dt else (), prev_iid=prev)


run('b_coarse20_10_fine20dt', [A0, A1, A2(), None, None])
run('b_coarse20_10_fine20df', [A0, A1, A2(dt=False), None, None])
run('b_coarse20_10_coarse20dt', [A0, A1, A2(mode=1), None, None])
run('b_coarse20_fine20dt', [A0, A2(prev=v20), None, None])
A1f = pdata(iid=[v10], iid_mode=3, icc=[icc10], icc_mode=0)
run('b_coarse20_fine10_fine20dt', [A0, A1f, A2(), None, None])
A1x4 = pdata(iid=[v10, walk(10, -7, 7), walk(10, -7, 7), v10], iid_mode=0,
             icc=[icc10, walk(10, 0, 7), walk(10, 0, 7), icc10], icc_mode=0)
run('b_coarse20_10x4_fine20dt', [A0, A1x4, A2(), None, None])
A0x4 = pdata(iid=[v20, walk(20, -7, 7), walk(20, -7, 7), v20], iid_mode=1, icc=[icc20] * 4,
             icc_mode=4, fc=1, borders=(3, 8, 16, 31))
run('b_fc1x4_10x4_fine20dt', [A0x4, A1x4, A2(), None, None])
run('b_fc1x4_10x4_fine20df', [A0x4, A1x4, A2(dt=False), None, None])
run('b_coarse20_10x4_fine20df', [A0, A1x4, A2(dt=False), None, None])
run('b_coarse20_10x4_coarse20df', [A0, A1x4, A2(mode=1, dt=False), None, None])
A1x4_20 = pdata(iid=[walk(20, -7, 7) for _ in range(4)], iid_mode=1, icc=[icc20] * 4, icc_mode=4)
run('b_coarse20_20x4_coarse20df', [A0, A1x4_20, A2(mode=1, dt=False), None, None])
A0x1fc1 = pdata(iid=[v20], iid_mode=1, icc=[icc20], icc_mode=4, fc=1, borders=(31,))
run('b_fc1x1_10x4_coarse20df', [A0x1fc1, A1x4, A2(mode=1, dt=False), None, None])
A0x2 = pdata(iid=[v20, v20], iid_mode=1, icc=[icc20] * 2, icc_mode=4)
run('b_coarse20x2_10x4_coarse20df', [A0x2, A1x4, A2(mode=1, dt=False), None, None])
run('b_10x4_coarse20df', [A1x4, A2(mode=1, dt=False), None, None])

# Hypothesis: the dt reference is kept per envelope index (an array of 34 per envelope,
# a frame writing the first n entries of each of its envelopes), and envelope 0 reads the
# array of the previous frame's last envelope. After a 1-envelope 20-band frame and a
# 4-envelope 10-band frame, entries 10..19 of envelope 3's array were never written (0).
# Test: the reference's output for a dt envelope coded against that array equals its
# output for the same values coded frequency-differentially.
ref_h = v10 + [0] * 10
th = [int(np.clip(k + x, -7, 7)) for k, x in zip(ref_h, rng.integers(-2, 3, 20))]


def ref_frames(spec, seed=41):
    p = os.path.join(out, 'o41_h.aac')
    build2(p, spec, seed=seed)
    return decode_log(p)[0]


ya = ref_frames([A0, A1x4, pdata(iid=[th], iid_mode=4, icc=[icc20], icc_mode=1, iid_dt=(0,), prev_iid=ref_h), None, None])
yb = ref_frames([A0, A1x4, pdata(iid=[th], iid_mode=4, icc=[icc20], icc_mode=1), None, None])
yc = ref_frames([A0, A1x4, pdata(iid=[th], iid_mode=4, icc=[icc20], icc_mode=1, iid_dt=(0,), prev_iid=kept), None, None])
print('  reference, dt against the per-envelope array vs df of the same values: %s' % fmt(per_frame(ya, yb)[:-1].max(1)))
print('  reference, dt against the decoder kept array vs df of the same values:  %s' % fmt(per_frame(yc, yb)[:-1].max(1)))
