#!/usr/bin/env python3
"""Native observation of the video output path on synthetic 4:2:0 frames (research tooling).
Writes random/ramp YUV frames, converts them with the reference filter chain and
saves both for fitting the conversion arithmetic."""
import subprocess, sys
import numpy as np
from pathlib import Path
ffmpeg, out = sys.argv[1], Path(sys.argv[2]); out.mkdir(parents=True, exist_ok=True)
rng = np.random.default_rng(280)
chain = 'scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,format=rgba'
for w, h in [(1920, 1080), (1280, 720), (3840, 2160), (720, 576), (1440, 1080)]:
    y = rng.integers(0, 256, (h, w), dtype=np.uint8)
    u = rng.integers(0, 256, (h // 2, w // 2), dtype=np.uint8)
    v = rng.integers(0, 256, (h // 2, w // 2), dtype=np.uint8)
    name = f'rand-{w}x{h}'
    (out / (name + '.yuv')).write_bytes(y.tobytes() + u.tobytes() + v.tobytes())
    for space, rng_tag in [('bt709', 'tv'), ('bt470bg', 'tv'), ('bt709', 'pc'), ('unknown', 'tv')]:
        args = [ffmpeg, '-v', 'error', '-y', '-f', 'rawvideo', '-pix_fmt', 'yuv420p', '-s', f'{w}x{h}', '-color_range', rng_tag]
        if space != 'unknown':
            args += ['-colorspace', space]
        args += ['-i', str(out / (name + '.yuv')), '-vf', chain, '-fps_mode', 'passthrough', '-f', 'rawvideo', '-pix_fmt', 'rgba',
                 str(out / f'{name}-{space}-{rng_tag}.rgba')]
        subprocess.run(args, check=True)
    print(name, flush=True)
