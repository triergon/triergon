"""Recovered model of the reference scaler's filter taps (research tooling).

Exact rational arithmetic; see research/d4/RGBA-CONTRACT.json for the observations.
taps(n, m) -> list of (start, integer coefficients) for each of the m outputs.

Rules (each recovered from black-box impulse measurements through probe_rgba.c; the Rust
implementation is crates/pixel/src/letterbox.rs Filter::new):
- step inc = round(n * 65536 / m); output x has centre c = (x * inc + (inc - 65536) / 2) / 65536.
- kernel Keys cubic a = -0.6 evaluated at d / s with s = max(n / m, 1) (exact ratio, not inc);
  window size = 4 (upscale) or ceil(4 s) taps, starting at floor(c - (size - 1) / 2 + 1 / 2);
  when that start is negative it is ceil(c - (size - 1) / 2) instead (left edge only).
- leading-tap trim, outputs visited from last to first: drop the first tap while the running
  sum of dropped |K| stays <= 1/3000 (raw kernel units) and the start stays below the next
  output's start.
- taps outside the source fold onto the nearest edge sample; normalise to `one`; quantise
  left to right with error diffusion (floor(v + 1/2), carry the remainder).
- horizontal (14-bit) passes round each kernel value to a multiple of 2^-30 first; vertical
  (12-bit) passes use it exactly. Only near-ties depend on this; one measured
  row (1056 -> 135, x = 121) still differs.
"""
from fractions import Fraction as Fr
import math

A = Fr(-3, 5)  # Keys cubic parameter (Mitchell-Netravali B=0, C=0.6)
CUTOFF = Fr(1, 3000)  # 0.002 of the kernel scaled by 6; brackets in RGBA-CONTRACT.json


def keys(d):
    d = abs(d)
    if d < 1:
        return ((A + 2) * d - (A + 3)) * d * d + 1
    if d < 2:
        return ((A * d - 5 * A) * d + 8 * A) * d - 4 * A
    return Fr(0)


def diffuse(w):
    out, err = [], Fr(0)
    for v in w:
        v += err
        r = math.floor(v + Fr(1, 2))
        err = v - r
        out.append(r)
    return out


KBITS_H, KBITS_V = 30, None
KMODE = 'round'  # experiment knob for the horizontal kernel rounding


def raw_windows(n, m, base=None, kbits=None):
    inc = ((n << 16) + m // 2) // m
    scale = max(Fr(n, m), Fr(1))
    size = math.ceil(4 * scale)  # window taps
    half = Fr(size - 1, 2)
    wins = []
    for x in range(m):
        c = ((Fr(inc - 65536, 2) if base is None else Fr(base)) + x * inc) / 65536
        s = math.floor(c - half + Fr(1, 2))
        if s < 0:
            s = math.ceil(c - half)
        w = [keys((k - c) / scale) for k in range(s, s + size)]
        if kbits is not None:  # horizontal passes: kernel rounded to 2^-30 (near-tie evidence)
            r = {'round': lambda x: math.floor(x + Fr(1, 2)), 'trunc': math.trunc, 'floor': math.floor, 'ceil': math.ceil,
                 'away': lambda x: math.copysign(math.floor(abs(x) + Fr(1, 2)), x)}[KMODE]
            w = [Fr(int(r(v * (1 << kbits))), 1 << kbits) for v in w]
        wins.append([s, w])
    return wins


def taps(n, m, one=16384, base=None, cutoff=CUTOFF, trim=True):
    wins = raw_windows(n, m, base, kbits=KBITS_H if one == 16384 else KBITS_V)
    if trim:
        for i in range(m - 1, -1, -1):
            s, w = wins[i]
            cut = Fr(0)
            while len(w) > 1:
                cut += abs(w[0])
                if cut > cutoff:  # absolute, raw kernel units (sum ~ scale)
                    break
                if i < m - 1 and s >= wins[i + 1][0]:
                    break
                w = w[1:]; s += 1
            wins[i] = [s, w]
    res = []
    for s, w in wins:
        lo, hi = max(s, 0), min(s + len(w), n)
        f = [Fr(0)] * (hi - lo)
        for j, v in enumerate(w):
            f[min(max(s + j, 0), n - 1) - lo] += v
        total = sum(f)
        res.append((lo, diffuse([v / total * one for v in f])))
    return res
