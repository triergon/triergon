"""o32 (case 3): fresh random streams of verify_compose's generator with frames without
the SBR element (its 'TS' kind), every other frame kind included, without 20 <-> 34
changes (p_switch 0), decoded by the reference and the builds given; whole streams from
frame 0, frames above 5e-8 listed.

usage: o32_no_sbr_random.py OUTDIR seed0 count binary [binary ...]"""
from olib import *

out, seed0, count = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
bins = sys.argv[4:]
os.makedirs(out, exist_ok=True)
VC.NOSBR = True
worst = [0.0] * len(bins)
nosbr_frames = 0
for seed in range(seed0, seed0 + count):
    rng = np.random.default_rng(seed)
    spec, model = VC.make_stream(rng, p_switch=0.0, start_cfg=(20, 34)[seed % 2], allow_change=True)
    nosbr_frames += sum(1 for s in spec if s == ('nosbr',))
    p = os.path.join(out, 'o32_%d.aac' % seed)
    build2(p, spec, seed=seed)
    y, msgs, _ = decode_log(p)
    line = []
    for i, b in enumerate(bins):
        z = rust(p, b)
        r = per_frame(z, y)[:-1].max(1)
        tot = float(np.sqrt(np.mean((z[:len(r) * 2048] - y[:len(r) * 2048]) ** 2)))
        worst[i] = max(worst[i], tot)
        line.append('rms %.2e above floor %s' % (tot, [f for f in range(len(r)) if r[f] > 5e-8]))
    kinds = ''.join('n' if s == ('nosbr',) else '.' for s in spec)
    print('seed %d %s: %s' % (seed, kinds, ' | '.join(line)))
print('%d frames without the SBR element; worst stream rms per build: %s' % (
    nosbr_frames, ', '.join('%.2e' % w for w in worst)))
