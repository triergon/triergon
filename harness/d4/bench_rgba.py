#!/usr/bin/env python3
"""Paired per-picture conversion timing: reference chain (probe_rgba PROBE_BENCH) vs crates/sim Scaler
(the ignored raw_letterbox test, release build), plus a byte comparison of both outputs (research tooling).
usage: bench_rgba.py WORKDIR [WxH:bits ...]   (run nothing else meanwhile; Mac numbers are dev evidence only)
"""
import sys, os, subprocess, json
sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import run, PROBE, ROOT

work = os.path.abspath(sys.argv[1]); os.makedirs(work, exist_ok=True)
env = dict(os.environ, CARGO_INCREMENTAL='0', CARGO_TARGET_DIR=str(ROOT / 'target/agent-rgba'))
subprocess.run(['cargo', 'test', '--release', '-q', '-p', 'triergon-sim', '--test', 'rgba_reference', '--no-run'], check=True, env=env, cwd=ROOT)
gen = np.random.default_rng(7)
for spec in sys.argv[2:] or ['1920x1080:8', '1280x720:8', '3840x2160:10', '1920x1080:10', '176x144:8', '1920x800:8']:
    size, bits = spec.split(':'); w, h = map(int, size.split('x')); bits = int(bits)
    # smooth-ish content: random walk images keep chroma/luma realistic but non-constant
    top = (1 << bits) - 1
    Y = np.clip(gen.normal(top / 2, top / 5, (h, w)), 0, top).astype(np.int64)
    U = np.clip(gen.normal(top / 2, top / 8, ((h + 1) // 2, (w + 1) // 2)), 0, top).astype(np.int64)
    V = np.clip(gen.normal(top / 2, top / 8, ((h + 1) // 2, (w + 1) // 2)), 0, top).astype(np.int64)
    dt = np.uint8 if bits == 8 else np.dtype('<u2')
    frame = b''.join(p.astype(dt).tobytes() for p in (Y, U, V))
    raw = os.path.join(work, f'{w}x{h}-{bits}.yuv'); open(raw, 'wb').write(frame * 30)
    cs, rng = 1, 1
    ref = subprocess.run([str(PROBE), str(w), str(h), str(bits), str(cs), str(rng)], input=frame, capture_output=True, check=True,
                         env=dict(os.environ, PROBE_BENCH='200')).stdout
    ref_t = json.loads(ref)
    out = os.path.join(work, f'{w}x{h}-{bits}.rgba')
    r = subprocess.run(['cargo', 'test', '--release', '-q', '-p', 'triergon-sim', '--test', 'rgba_reference', '--', '--ignored', 'raw_letterbox', '--nocapture', '--test-threads=1'],
                       env=dict(env, RGBA_RAW=f'{w} {h} {bits} {cs} 0 {raw} {out}', RGBA_BENCH='200'), capture_output=True, text=True, cwd=ROOT)
    ours = [l for l in r.stderr.splitlines() if l.startswith('frames')]
    ours_ns = int(ours[0].split()[-1]) if ours else -1
    subprocess.run(['cargo', 'test', '--release', '-q', '-p', 'triergon-sim', '--test', 'rgba_reference', '--', '--ignored', 'raw_letterbox'],
                   env=dict(env, RGBA_RAW=f'{w} {h} {bits} {cs} 0 {raw} {out}'), capture_output=True, cwd=ROOT, check=True)
    mine = np.frombuffer(open(out, 'rb').read(), dtype=np.uint8)[:1920 * 1080 * 4]
    want = run([(Y, U, V)], bits=bits, cs=cs, rng=rng)[0].reshape(-1)
    bad = int((mine.reshape(-1, 4) != want.reshape(-1, 4)).any(1).sum())
    print(f'{spec}: reference median {ref_t["median_ns"] / 1e6:.2f} ms, ours median {ours_ns / 1e6:.2f} ms, ratio {ours_ns / ref_t["median_ns"]:.2f}, mismatched pixels {bad}', flush=True)
    os.remove(raw); os.remove(out)
