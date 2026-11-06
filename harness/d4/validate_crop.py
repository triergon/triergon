#!/usr/bin/env python3
"""Left/top-cropped vectors against reference_decode itself (research tooling, dev Mac).

The reference decoder keeps part of a left crop (alignment rule, see crates/sim scale.rs
reference_crop), so cropped planes from the FFmpeg CLI are not what its graph sees. Here the
Scaler gets whole coded planes plus the crop offset (the layout decoders hand over, codec::Plane)
and its canvases are compared with reference_decode raw output.
usage: validate_crop.py VECTOR LANE(h264|hevc) [...]   needs target/agent-rgba/{reference_decode,probe_crop}
"""
import sys, os, re, json, subprocess
sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import ROOT

T = ROOT / 'target/agent-rgba'
env = dict(os.environ, CARGO_INCREMENTAL='0', CARGO_TARGET_DIR=str(T))
build = subprocess.run(['cargo', 'test', '--release', '-p', 'triergon-sim', '--test', 'rgba_reference', '--no-run', '--message-format=json'],
                       env=env, cwd=ROOT, capture_output=True, text=True, check=True)
binary = [json.loads(l)['executable'] for l in build.stdout.splitlines() if '"executable":"' in l][0]
args = sys.argv[1:]
for path, lane in zip(args[::2], args[1::2]):
    info = subprocess.run([str(T / 'probe_crop'), path, '1'], capture_output=True, text=True, check=True).stdout
    cw, ch, l, r, t, b = map(int, re.search(r'coded (\d+)x(\d+) crop l=(\d+) r=(\d+) t=(\d+) b=(\d+)', info).groups())
    st = json.loads(subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries', 'stream=pix_fmt,color_space,color_range',
                                    '-of', 'json', path], capture_output=True, text=True).stdout)['streams'][0]
    bits = 10 if st['pix_fmt'].endswith('10le') else 8
    cs = {'bt709': 1, 'fcc': 4, 'bt470bg': 5, 'smpte170m': 6, 'smpte240m': 7, 'bt2020nc': 9}.get(st.get('color_space', ''), 2)
    full = int(st.get('color_range') == 'pc')
    ref, yuv, ours = T / 'crop-ref.rgba', T / 'crop.yuv', T / 'crop-ours.rgba'
    subprocess.run([str(T / 'reference_decode'), f'video-{lane}-raw', path, str(ref)], check=True, capture_output=True)
    subprocess.run(['ffmpeg', '-v', 'error', '-y', '-apply_cropping', 'none', '-i', path, '-f', 'rawvideo', '-pix_fmt',
                    'yuv420p10le' if bits == 10 else 'yuv420p', str(yuv)], check=True)
    w, h = cw - l - r, ch - t - b
    subprocess.run([binary, '--ignored', 'raw_letterbox', '--test-threads=1'], check=True, capture_output=True,
                   env=dict(env, RGBA_RAW=f'{w} {h} {bits} {cs} {full} {yuv} {ours} {cw} {ch} {l} {t}'))
    a = np.fromfile(ref, np.uint8).reshape(-1, 1080, 1920, 4); o = np.fromfile(ours, np.uint8).reshape(-1, 1080, 1920, 4)
    n = min(len(a), len(o)); d = (a[:n] != o[:n]).any(-1)
    print(os.path.basename(path), f'crop l{l} r{r} t{t} b{b}', 'pictures', len(a), len(o), 'differing', int(d.any((1, 2)).sum()),
          'bad px', int(d.sum()), flush=True)
    for f in (ref, yuv, ours): os.remove(f)
