#!/usr/bin/env python3
"""Validate triergon_pixel::letterbox::Scaler on real decoded pictures (research tooling, dev Mac).

For each video vector, FFmpeg decodes to raw planes (the same pictures the reference decoder hands
its filter graph); chunks of pictures go through (a) the reference chain (probe_rgba, identical to
the decode reference's graph, colorspace/range as the decoder reports them) and (b) the Scaler
(release `raw_letterbox` hook). Prints per-vector exactness. Converter-only: decoder differences
are excluded by construction.
usage: validate_rgba.py OUT.json [VECTOR ...]   (default: every h264/hevc vector of
       harness/codecs/correctness-manifest.json plus the .research/codec-inputs workloads)
env: MAX_FRAMES (default 40 per vector), CHUNK (default 8)
"""
import sys, os, json, subprocess, glob
sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import PROBE, ROOT, fit

MAX = int(os.environ.get('MAX_FRAMES', '40')); CHUNK = int(os.environ.get('CHUNK', '8'))
work = ROOT / 'target/agent-rgba/val'; work.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, CARGO_INCREMENTAL='0', CARGO_TARGET_DIR=str(ROOT / 'target/agent-rgba'))
build = subprocess.run(['cargo', 'test', '--release', '-p', 'triergon-pixel', '--test', 'rgba_reference', '--no-run', '--message-format=json'],
                       env=env, cwd=ROOT, capture_output=True, text=True, check=True)
binary = [json.loads(l)['executable'] for l in build.stdout.splitlines() if '"executable":"' in l][0]
CS = {'bt709': 1, 'fcc': 4, 'bt470bg': 5, 'smpte170m': 6, 'smpte240m': 7, 'bt2020nc': 9, 'bt2020c': 10, 'gbr': 0}
RANGE = {'tv': 1, 'pc': 2}


def vectors():
    if len(sys.argv) > 2:
        return sys.argv[2:]
    m = json.load(open(ROOT / 'harness/codecs/correctness-manifest.json'))
    out = [str(ROOT / v['path']) for v in m['vectors'] if v['lane'] in ('h264', 'hevc')]
    return out + sorted(glob.glob(str(ROOT / '.research/codec-inputs/*.h264')) + glob.glob(str(ROOT / '.research/codec-inputs/*.hevc')))


results = {}
for path in vectors():
    name = os.path.basename(path)
    fmt = 'hevc' if path.endswith(('.bit', '.hevc')) or '/hevc-' in path else 'h264'
    probe = subprocess.run(['ffprobe', '-v', 'error', '-f', fmt, '-select_streams', 'v:0', '-show_entries',
                            'stream=width,height,pix_fmt,color_space,color_range', '-of', 'json', path], capture_output=True, text=True)
    try:
        st = json.loads(probe.stdout)['streams'][0]
    except (KeyError, IndexError, json.JSONDecodeError):
        results[name] = {'status': 'ffprobe failed'}; continue
    w, h, pix = st.get('width', 0), st.get('height', 0), st.get('pix_fmt', '')
    if pix not in ('yuv420p', 'yuv420p10le', 'yuvj420p'):
        results[name] = {'status': f'skipped: {pix} is outside the 4:2:0 8/10-bit scope'}; continue
    bits = 10 if pix.endswith('10le') else 8
    cs = CS.get(st.get('color_space', ''), 2)
    rng = 2 if pix == 'yuvj420p' else RANGE.get(st.get('color_range', ''), 0)
    if cs in (8, 10, 11, 12, 13, 14):
        results[name] = {'status': 'skipped: reference refuses this matrix'}; continue
    raw = subprocess.run(['ffmpeg', '-v', 'error', '-threads', '1', '-f', fmt, '-i', path, '-frames:v', str(MAX), '-fps_mode', 'passthrough',
                          '-f', 'rawvideo', '-pix_fmt', 'yuv420p10le' if bits == 10 else 'yuv420p', '-'], capture_output=True).stdout
    cw, ch = (w + 1) // 2, (h + 1) // 2
    fbytes = (w * h + 2 * cw * ch) * (2 if bits == 10 else 1)
    n = len(raw) // fbytes
    fw, fh, fx, fy = fit(w, h)
    stats = {'size': f'{w}x{h}', 'bits': bits, 'matrix': cs, 'range': rng, 'fit': f'{fw}x{fh}+{fx}+{fy}', 'frames': n,
             'exact_frames': 0, 'bad_pixels': 0, 'max_abs': 0}
    for c0 in range(0, n, CHUNK):
        chunk = raw[c0 * fbytes:min(n, c0 + CHUNK) * fbytes]
        k = len(chunk) // fbytes
        ref = subprocess.run([str(PROBE), str(w), str(h), str(bits), str(cs), str(rng)], input=chunk, capture_output=True, check=True).stdout
        inp, outp = work / 'in.yuv', work / 'out.rgba'
        inp.write_bytes(chunk)
        subprocess.run([binary, '--ignored', 'raw_letterbox', '--exact'], env=dict(env, RGBA_RAW=f'{w} {h} {bits} {cs} {int(rng == 2)} {inp} {outp}'),
                       capture_output=True, check=True)
        a = np.frombuffer(ref, dtype=np.uint8).reshape(k, -1, 4); b = np.frombuffer(outp.read_bytes(), dtype=np.uint8).reshape(k, -1, 4)
        d = (a != b).any(-1)
        stats['exact_frames'] += int((~d.any(-1)).sum()); stats['bad_pixels'] += int(d.sum())
        stats['max_abs'] = max(stats['max_abs'], int(np.abs(a.astype(int) - b.astype(int)).max()))
        os.remove(inp); os.remove(outp)
    stats['status'] = 'exact' if stats['exact_frames'] == n and n else 'differs'
    results[name] = stats
    print(name, stats, flush=True)
json.dump(results, open(sys.argv[1], 'w'), indent=1)
summary = {}
for r in results.values():
    summary[r['status'].split(':')[0]] = summary.get(r['status'].split(':')[0], 0) + 1
print('summary', summary)
