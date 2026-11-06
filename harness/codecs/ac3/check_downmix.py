#!/usr/bin/env python3
"""Per-frame comparison of FrameInfo.downmix (examples/decode with DMXALL=1) against
FFmpeg's AV_FRAME_DATA_DOWNMIX_INFO as printed by `ffprobe -show_frames` (dB, 0.1 dB).

usage: python3 -I check_downmix.py <path/to/examples/decode> <stream>...
"""
import math
import os
import subprocess
import sys

MP = {'center': 'center_mix_level_db', 'center_ltrt': 'center_mix_level_ltrt_db',
      'surround': 'surround_mix_level_db', 'surround_ltrt': 'surround_mix_level_ltrt_db',
      'lfe': 'lfe_mix_level_db'}
PREF = {'Unknown': 'unknown', 'LoRo': 'loro', 'LtRt': 'ltrt', 'DplII': 'dplII'}


def db(x):
    x = float(x)
    return '-inf' if x == 0 else '%.1f' % (20 * math.log10(x))


dec = sys.argv[1]
for f in sys.argv[2:]:
    r = subprocess.run([dec, f, '/dev/null'], env=dict(os.environ, DMXALL='1'), stderr=subprocess.PIPE, text=True)
    ours = [l[4:].strip() for l in r.stderr.splitlines() if l.startswith('DMX ')]
    p = subprocess.run(['ffprobe', '-v', 'error', '-show_frames', '-of', 'compact', f], capture_output=True, text=True)
    ff = []
    for l in p.stdout.splitlines():
        if not l.startswith('frame'):
            continue
        kv = {}
        for item in l.split('|'):
            if '=' in item:
                k, v = item.split('=', 1)
                kv[k.split(':')[-1]] = v
        ff.append(kv if 'center_mix_level_db' in kv else None)
    bad = 0
    for a, b in zip(ours, ff):
        if a == 'none' or b is None:
            bad += (a == 'none') != (b is None)
            continue
        x = dict(kv.split('=') for kv in a.split())
        ok = all(db(x[k]) == b[v] for k, v in MP.items()) and PREF[x['pref']] == b['preferred_downmix_type']
        bad += not ok
    example = ours[min(2, len(ours) - 1)] if ours else '-'
    print('%s: frames ours %d ffprobe %d, mismatching frames %d; e.g. %s'
          % (os.path.basename(f), len(ours), len(ff), bad, example))
