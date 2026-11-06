"""Shared helpers for the D4 RGBA black-box probes (research tooling).

run(planes, bits, cs, rng, chain=None) feeds 4:2:0 frames to harness/d4/probe_rgba
(built at target/agent-rgba/probe_rgba) and returns the output frames as numpy arrays.
"""
import os, subprocess
import numpy as np
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROBE = Path(os.environ.get('PROBE_RGBA', ROOT / 'target/agent-rgba/probe_rgba'))
CANVAS = (1080, 1920)


def run(frames, bits=8, cs=2, rng=1, chain=None, out_shape=None, out_bpp=4):
    """frames: list of (Y, U, V) integer arrays. Returns array (N, H, W, out_bpp) of uint8."""
    h, w = frames[0][0].shape
    dt = np.uint8 if bits == 8 else np.dtype('<u2')
    blob = b''.join(p.astype(dt).tobytes() for f in frames for p in f)
    env = dict(os.environ)
    if chain:
        env['PROBE_CHAIN'] = chain
    out = subprocess.run([str(PROBE), str(w), str(h), str(bits), str(cs), str(rng)], input=blob,
                         stdout=subprocess.PIPE, check=True, env=env).stdout
    oh, ow = out_shape or CANVAS
    return np.frombuffer(out, dtype=np.uint8).reshape(len(frames), oh, ow, out_bpp)


def fit(w, h):
    """Fitted size and offset (triergon_pixel::letterbox::fit)."""
    fw = min((1080 * w + h // 2) // h, 1920)
    fh = min((1920 * h + w // 2) // w, 1080)
    return fw, fh, (1920 - fw) // 2, (1080 - fh) // 2
