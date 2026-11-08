#!/usr/bin/env python3
"""Generate the synthetic FLAC vectors of the flac lane of cargo xtask codecs grade.

    flac_vectors.py FFMPEG FLAC OUT_DIR

Writes deterministic synthetic signals as WAV and encodes them with FFMPEG's FLAC
encoder and the reference FLAC encoder over a matrix of bit depths, channel counts,
sample rates, block sizes and encoder options, into OUT_DIR (the manifest's
.research/flac-generated). The vectors are identified by their SHA-256 in
harness/codecs/correctness-manifest.json. Media stays outside git.
"""
import math
import os
import random
import struct
import subprocess
import sys
import tempfile


# --------------------------------------------------------------------------- signals

DEFAULT_MASKS = {1: 0x4, 2: 0x3, 3: 0x7, 4: 0x33, 5: 0x607, 6: 0x60F, 7: 0x70F, 8: 0x63F}


def signal(seed, channels, bits, rate, seconds=1.2, wasted=0):
    """Per channel: tones, noise, a silent stretch (constant subframes), a white-noise
    burst (verbatim/escaped partitions) and a full-scale square stretch. Channel 1 is
    mostly channel 0, so stereo decorrelation pays off."""
    rng = random.Random(seed)
    n = int(rate * seconds)
    peak = (1 << (bits - 1)) - 1
    step = 1 << wasted
    out = []
    base = []
    for c in range(channels):
        freqs = [rng.uniform(40, min(8000, rate / 3)) for _ in range(3)]
        phases = [rng.uniform(0, 2 * math.pi) for _ in range(3)]
        plane = []
        for i in range(n):
            t = i / rate
            pos = i / n
            if 0.30 < pos < 0.36:
                v = 0.0
            elif 0.50 < pos < 0.53:
                v = rng.uniform(-1, 1)
            elif 0.70 < pos < 0.72:
                v = 1.0 if (i // 37) % 2 else -1.0
            else:
                v = sum(0.28 * math.sin(2 * math.pi * f * t + p) for f, p in zip(freqs, phases))
                v += rng.gauss(0, 0.01)
            if c == 1 and base:
                v = 0.9 * base[i] + 0.1 * v
            plane.append(v)
        if c == 0:
            base = plane
        out.append([max(-peak - 1, min(peak, int(round(v * peak)) // step * step)) for v in plane])
    return out


def write_wav(path, planes, bits, rate, mask=None):
    """WAVE_FORMAT_EXTENSIBLE, container of whole bytes, valid bits = `bits`."""
    channels = len(planes)
    container = (bits + 7) // 8
    shift = container * 8 - bits
    frames = bytearray()
    for i in range(len(planes[0])):
        for plane in planes:
            v = plane[i] << shift
            if container == 1:
                frames += struct.pack("<B", (v + 128) & 0xFF)
            else:
                frames += (v & ((1 << (8 * container)) - 1)).to_bytes(container, "little")
    mask = DEFAULT_MASKS[channels] if mask is None else mask
    fmt = struct.pack(
        "<HHIIHHHHI16s",
        0xFFFE,
        channels,
        rate,
        rate * channels * container,
        channels * container,
        container * 8,
        22,
        bits,
        mask,
        b"\x01\x00\x00\x00\x00\x00\x10\x00\x80\x00\x00\xaa\x00\x38\x9b\x71",
    )
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 4 + 8 + len(fmt) + 8 + len(frames)) + b"WAVE")
        f.write(b"fmt " + struct.pack("<I", len(fmt)) + fmt)
        f.write(b"data" + struct.pack("<I", len(frames)) + frames)


# --------------------------------------------------------------------------- matrix


def matrix():
    """(name, bits, channels, rate, encoder, options, signal extras)."""
    cases = []
    add = lambda *case: cases.append(case)
    # FFmpeg's encoder.
    for ch in range(1, 9):
        add(f"ff_s16_ch{ch}", 16, ch, 44100, "ffmpeg", [], {})
    for level in range(13):
        add(f"ff_s16_level{level}", 16, 2, 44100, "ffmpeg", ["-compression_level", str(level)], {})
    for lpc in ["none", "fixed", "levinson", "cholesky"]:
        add(f"ff_s16_lpc_{lpc}", 16, 2, 48000, "ffmpeg", ["-lpc_type", lpc], {})
    for mode in ["indep", "left_side", "right_side", "mid_side"]:
        add(f"ff_s16_{mode}", 16, 2, 44100, "ffmpeg", ["-ch_mode", mode], {})
        add(f"ff_s24_{mode}", 24, 2, 96000, "ffmpeg", ["-ch_mode", mode], {})
        add(f"ff_s32_{mode}", 32, 2, 48000, "ffmpeg", ["-ch_mode", mode], {})
    add("ff_s16_exact_rice", 16, 2, 44100, "ffmpeg", ["-exact_rice_parameters", "1"], {})
    add("ff_s16_multi_dim_quant", 16, 2, 44100, "ffmpeg", ["-multi_dim_quant", "1"], {})
    for lo, hi in [(0, 0), (8, 8), (0, 8)]:
        add(f"ff_s16_partitions_{lo}_{hi}", 16, 2, 44100, "ffmpeg",
            ["-min_partition_order", str(lo), "-max_partition_order", str(hi)], {})
    for method in ["estimation", "2level", "search", "log"]:
        add(f"ff_s16_order_{method}", 16, 2, 44100, "ffmpeg",
            ["-compression_level", "8", "-prediction_order_method", method], {})
    for size in [16, 192, 576, 4608, 16384, 32768]:
        add(f"ff_s16_block{size}", 16, 2, 44100, "ffmpeg", ["-frame_size", str(size)], {})
    for order in [1, 12, 32]:
        add(f"ff_s24_order{order}", 24, 2, 96000, "ffmpeg",
            ["-lpc_type", "levinson", "-min_prediction_order", str(order),
             "-max_prediction_order", str(order), "-lpc_coeff_precision", "15"], {})
    add("ff_s32_order32", 32, 2, 48000, "ffmpeg",
        ["-lpc_type", "cholesky", "-max_prediction_order", "32", "-lpc_coeff_precision", "15"], {})
    for ch in [1, 6, 8]:
        add(f"ff_s24_ch{ch}", 24, ch, 48000, "ffmpeg", ["-compression_level", "8"], {})
        add(f"ff_s32_ch{ch}", 32, ch, 48000, "ffmpeg", [], {})
    for rate in [8000, 11025, 22050, 32000, 37800, 88200, 176400, 192000]:
        add(f"ff_s16_rate{rate}", 16, 2, rate, "ffmpeg", [], {})
    add("ff_s16_wasted2", 16, 2, 44100, "ffmpeg", [], {"wasted": 2})
    add("ff_s24_wasted8", 24, 2, 48000, "ffmpeg", [], {"wasted": 8})
    # The reference encoder.
    for bits in [4, 5, 8, 12, 15, 16, 20, 24, 28, 31, 32]:
        lax = [] if bits in (8, 12, 16, 20, 24, 32) else ["--lax"]
        add(f"flac_b{bits}_stereo", bits, 2, 44100, "flac", ["-8", *lax], {})
    for bits in [8, 12, 20]:
        add(f"flac_b{bits}_ch7", bits, 7, 48000, "flac", ["-5"], {})
    for ch in range(1, 9):
        add(f"flac_s16_ch{ch}", 16, ch, 44100, "flac", ["-8"], {})
    for level in [0, 3, 5]:
        add(f"flac_s16_level{level}", 16, 2, 44100, "flac", [f"-{level}"], {})
    add("flac_s16_exhaustive", 16, 2, 44100, "flac", ["-8", "-e", "-p"], {})
    add("flac_s16_no_mid_side", 16, 2, 44100, "flac", ["-8", "--no-mid-side"], {})
    add("flac_s16_adaptive_mid_side", 16, 2, 44100, "flac", ["-5", "-M"], {})
    add("flac_s24_lax_order32", 24, 2, 96000, "flac", ["--lax", "-l", "32", "-q", "15", "-r", "15"], {})
    add("flac_s32_lax_order32", 32, 2, 48000, "flac", ["--lax", "-l", "32", "-r", "15"], {})
    add("flac_s16_block65535", 16, 2, 44100, "flac", ["--lax", "-b", "65535"], {})
    add("flac_s16_block16", 16, 1, 44100, "flac", ["--lax", "-b", "16", "-l", "8"], {})
    add("flac_s16_partition15", 16, 1, 44100, "flac", ["--lax", "-b", "32768", "-r", "15"], {})
    add("flac_s16_precision5", 16, 2, 44100, "flac", ["-8", "-q", "5"], {})
    add("flac_s16_wasted3", 16, 2, 44100, "flac", ["-8"], {"wasted": 3})
    add("flac_s20_rate768k", 20, 2, 768000, "flac", ["--lax", "-5"], {})
    add("flac_s16_rate1", 16, 1, 1, "flac", ["--lax", "-5"], {"seconds": 400})
    add("flac_quad_side_mask", 16, 4, 44100, "flac", ["-8", "--channel-map=none"], {"mask": 0x603})
    add("flac_5ch_back_mask", 16, 5, 44100, "flac", ["-8", "--channel-map=none"], {"mask": 0x37})
    add("flac_3ch_odd_mask", 16, 3, 44100, "flac", ["-8", "--channel-map=none"], {"mask": 0x30001})
    return cases


def generate(ffmpeg, flac, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        for seed, (name, bits, ch, rate, encoder, options, extra) in enumerate(matrix()):
            target = os.path.join(out_dir, name + ".flac")
            planes = signal(seed, ch, bits, rate, extra.get("seconds", 1.2), extra.get("wasted", 0))
            wav = os.path.join(tmp, name + ".wav")
            write_wav(wav, planes, bits, rate, extra.get("mask"))
            if encoder == "ffmpeg":
                fmt = "s16" if bits <= 16 else "s32"
                cmd = [ffmpeg, "-nostdin", "-v", "error", "-y", "-i", wav, "-c:a", "flac",
                       "-sample_fmt", fmt, "-bits_per_raw_sample", str(bits)]
                if bits == 32:
                    cmd += ["-strict", "experimental"]
                cmd += options + [target]
            else:
                cmd = [flac, "-s", "-f"] + options + ["-o", target, wav]
            result = subprocess.run(cmd, capture_output=True, text=True)
            status = "ok" if result.returncode == 0 else "FAILED " + result.stderr.strip()[:200]
            print(f"{name}: {status}")


def main(argv):
    if len(argv) != 3:
        print(__doc__)
        return 2
    generate(*argv)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
