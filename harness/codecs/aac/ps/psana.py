"""Decoding and measurement helpers for the Parametric Stereo probes.

`decode` runs the reference decoder (FFMPEG, default `ffmpeg` on PATH) on an ADTS stream
and returns float32 samples as an (n, channels) array. `qmf_bands` is a 64-band complex
cosine-modulated analysis with the 640-tap window of crates/codec-aac/src/sbr/tables.rs,
used here only to measure band energies, not to reproduce a decoder.
"""
import os
import re
import subprocess

import numpy as np

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..', '..'))
FFMPEG = os.environ.get('FFMPEG', 'ffmpeg')


def decode(path, channels=2):
    out = subprocess.run([FFMPEG, '-v', 'error', '-f', 'aac', '-i', path, '-f', 'f32le',
                          '-c:a', 'pcm_f32le', '-'], check=True, capture_output=True).stdout
    return np.frombuffer(out, dtype='<f4').reshape(-1, channels).astype(np.float64)


def probe_channels(path):
    out = subprocess.run([FFMPEG.replace('ffmpeg', 'ffprobe'), '-v', 'error', '-show_entries',
                          'stream=channels,sample_rate', '-of', 'csv=p=0', path],
                         check=True, capture_output=True, text=True).stdout
    return out.strip()


def qmf_window():
    text = open(os.path.join(ROOT, 'crates', 'codec-aac', 'src', 'sbr', 'tables.rs')).read()
    m = re.search(r'QMF_WINDOW: \[f64; 640\] = \[(.*?)\];', text, re.S)
    vals = [float(v) for v in re.findall(r'-?\d+\.\d+(?:e-?\d+)?', m.group(1))]
    assert len(vals) == 640
    return np.array(vals)[::-1]


_WIN = None


def qmf_bands(x):
    """64-band analysis of a real signal: returns (slots, 64) complex."""
    global _WIN
    if _WIN is None:
        _WIN = qmf_window()
    c = _WIN
    n = np.arange(128)
    k = np.arange(64)
    mod = np.exp(1j * np.pi * np.outer(k + 0.5, 2 * n - 0.5) / 128)
    slots = (len(x) - 640) // 64
    out = np.empty((slots, 64), complex)
    for t in range(slots):
        seg = x[64 * t:64 * t + 640][::-1] * c
        u = seg.reshape(5, 128).sum(0)
        out[t] = mod @ u
    return out
