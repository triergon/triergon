#!/usr/bin/env python3
"""Grade a decoder output against the reference oracle's output.

usage: compare.py REFERENCE.f32 CANDIDATE.f32 [NAME]

Both files hold interleaved f32le frames, each with its own channel count; the frame
list comes from REFERENCE.f32.frames ("samples channels rate skip_start skip_end" per
line, written by harness/codecs/mp3/reference.c). Prints one JSON line: exact count checks and, per
channel, RMS and maximum absolute difference, graded with the rules of
harness/codecs/numeric-rules.json (RMS <= 8.8e-6, max |d| <= 6.1035e-5).
"""
import json
import struct
import sys
from array import array

RMS_MAX = 8.8e-06
ABS_MAX = 6.1035e-05


def load(path):
    data = array("f")
    with open(path, "rb") as f:
        raw = f.read()
    data.frombytes(raw[: len(raw) // 4 * 4])
    if sys.byteorder != "little":
        data.byteswap()
    return data


def main():
    ref_path, cand_path = sys.argv[1], sys.argv[2]
    name = sys.argv[3] if len(sys.argv) > 3 else cand_path
    frames = []
    with open(ref_path + ".frames") as f:
        for line in f:
            samples, channels, rate = (int(x) for x in line.split()[:3])
            frames.append((samples, channels, rate))
    ref, cand = load(ref_path), load(cand_path)
    result = {"name": name, "ref_frames": len(frames), "ref_values": len(ref), "cand_values": len(cand)}
    if len(ref) != len(cand):
        result.update(status="FAIL", reason="sample count")
        print(json.dumps(result))
        return
    sums, peaks, counts = [0.0, 0.0], [0.0, 0.0], [0, 0]
    at = 0
    for samples, channels, _ in frames:
        for c in range(channels):
            s = 0.0
            p = peaks[c]
            for i in range(at + c, at + samples * channels, channels):
                d = cand[i] - ref[i]
                s += d * d
                if abs(d) > p:
                    p = abs(d)
            sums[c] += s
            peaks[c] = p
            counts[c] += samples
        at += samples * channels
    rms = [(sums[c] / counts[c]) ** 0.5 if counts[c] else 0.0 for c in range(2)]
    ok = all(r <= RMS_MAX for r in rms) and all(p <= ABS_MAX for p in peaks)
    result.update(
        status="PASS" if ok else "FAIL",
        rms=[float("%.3g" % r) for r in rms],
        max_abs=[float("%.3g" % p) for p in peaks],
    )
    print(json.dumps(result))


if __name__ == "__main__":
    main()
