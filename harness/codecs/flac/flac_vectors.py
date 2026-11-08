#!/usr/bin/env python3
"""Generate synthetic FLAC vectors and grade the decoder against FFmpeg, bit-exact.

    flac_vectors.py generate OUT_DIR
    flac_vectors.py grade DECODER VECTOR_DIR... [--ffmpeg PATH] [--ffprobe PATH]

`generate` writes deterministic synthetic signals as WAV and encodes them with FFmpeg's
FLAC encoder and the reference `flac` encoder over a matrix of bit depths, channel
counts, sample rates, block sizes and encoder options. Media stays outside git.

`grade` decodes every .flac below the given directories with DECODER (the decode_flac
example) and with `ffmpeg -f f32le -c:a pcm_f32le`, and compares the output bytes, the
per-frame layouts and sample counts (ffprobe -show_frames) and the stream sample rate.
A stream whose sample rate or channel count changes is graded run by run, because the
FFmpeg command line resamples and remixes such a stream to its first format; FLAC frames
are independent, so each run decodes alike on its own.
"""
import json
import math
import os
import random
import struct
import subprocess
import sys
import tempfile

FFMPEG = "/opt/homebrew/bin/ffmpeg"
FFPROBE = "/opt/homebrew/bin/ffprobe"
FLAC = "flac"

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


def generate(out_dir):
    os.makedirs(out_dir, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        for seed, (name, bits, ch, rate, encoder, options, extra) in enumerate(matrix()):
            target = os.path.join(out_dir, name + ".flac")
            planes = signal(seed, ch, bits, rate, extra.get("seconds", 1.2), extra.get("wasted", 0))
            wav = os.path.join(tmp, name + ".wav")
            write_wav(wav, planes, bits, rate, extra.get("mask"))
            if encoder == "ffmpeg":
                fmt = "s16" if bits <= 16 else "s32"
                cmd = [FFMPEG, "-nostdin", "-v", "error", "-y", "-i", wav, "-c:a", "flac",
                       "-sample_fmt", fmt, "-bits_per_raw_sample", str(bits)]
                if bits == 32:
                    cmd += ["-strict", "experimental"]
                cmd += options + [target]
            else:
                cmd = [FLAC, "-s", "-f"] + options + ["-o", target, wav]
            result = subprocess.run(cmd, capture_output=True, text=True)
            status = "ok" if result.returncode == 0 else "FAILED " + result.stderr.strip()[:200]
            print(f"{name}: {status}")


# --------------------------------------------------------------------------- grading


def run(cmd):
    return subprocess.run(cmd, capture_output=True)


def ffprobe_json(path, entries, extra=()):
    out = run([FFPROBE, "-v", "error", "-select_streams", "a:0", *extra, "-show_entries", entries,
               "-of", "json", path])
    try:
        return json.loads(out.stdout or b"{}")
    except json.JSONDecodeError:
        return {}


def reference(path, out, fmt=()):
    result = run([FFMPEG, "-nostdin", "-v", "error", "-threads", "1", *fmt, "-i", path,
                  "-f", "f32le", "-c:a", "pcm_f32le", "-y", out])
    return result.returncode


def channel_runs(path):
    """Packets of `path` grouped into runs of equal (sample rate code, channel count)."""
    data = open(path, "rb").read()
    packets = ffprobe_json(path, "packet=pos,size").get("packets", [])
    runs = []
    for p in packets:
        if "pos" not in p:
            continue
        pos, size = int(p["pos"]), int(p["size"])
        head = data[pos:pos + 4]
        if len(head) < 4 or head[0] != 0xFF or head[1] & 0xFE != 0xF8:
            continue  # data before the first frame
        assignment = head[3] >> 4
        key = (head[2] & 15, assignment + 1 if assignment < 8 else 2)
        if runs and runs[-1][0] == key:
            runs[-1][1].append(data[pos:pos + size])
        else:
            runs.append((key, [data[pos:pos + size]]))
    return runs


def compare(candidate, expected, channels):
    """(exact, worst rms, worst abs, first differing sample) over interleaved f32le."""
    a = open(candidate, "rb").read()
    b = open(expected, "rb").read()
    if a == b:
        return True, 0.0, 0.0, None
    n = min(len(a), len(b)) // 4
    xa = struct.unpack(f"<{n}f", a[: 4 * n])
    xb = struct.unpack(f"<{n}f", b[: 4 * n])
    channels = max(channels, 1)
    sums = [0.0] * channels
    worst = 0.0
    first = None
    for i, (u, v) in enumerate(zip(xa, xb)):
        d = u - v
        if d != 0 and first is None:
            first = i // channels
        sums[i % channels] += d * d
        worst = max(worst, abs(d))
    per = n // channels or 1
    rms = max(math.sqrt(s / per) for s in sums)
    return False, rms, worst, first


def grade_file(decoder, path, tmp):
    cand = os.path.join(tmp, "c.f32")
    ref = os.path.join(tmp, "r.f32")
    dec = run([decoder, path, cand, "--per-frame"])
    lines = [json.loads(l) for l in dec.stderr.decode().splitlines() if l.startswith("{")]
    summary = lines[-1] if lines else {}
    frames = [l for l in lines[:-1] if "frame" in l and "error" not in l]
    notes = []
    stream = ffprobe_json(path, "stream=sample_rate,channels,channel_layout").get("streams", [{}])
    stream = stream[0] if stream else {}
    if reference(path, ref) != 0 and not summary.get("frames"):
        # FFmpeg refuses the stream and the decoder decodes none of it.
        return True, 0.0, 0.0, summary, ["refused by both"]
    probe_frames = ffprobe_json(path, "frame=channel_layout,nb_samples").get("frames", [])
    exp_layouts = [f.get("channel_layout") for f in probe_frames]
    got_layouts = [f["layout"] for f in frames]
    if exp_layouts != got_layouts:
        notes.append(f"layouts {summarise(got_layouts)} vs {summarise(exp_layouts)}")
    exp_counts = [int(f.get("nb_samples", -1)) for f in probe_frames]
    got_counts = [f["samples"] for f in frames]
    if exp_counts != got_counts:
        notes.append(f"frame sample counts differ ({len(got_counts)} vs {len(exp_counts)} frames)")
    runs = channel_runs(path)
    exact = True
    rms = worst = 0.0
    if len(runs) <= 1:
        if summary.get("sample_rate") != int(stream.get("sample_rate", 0)):
            notes.append(f"rate {summary.get('sample_rate')} vs {stream.get('sample_rate')}")
        exact, rms, worst, first = compare(cand, ref, summary.get("channels", 1))
        if not exact:
            notes.append(f"first difference at sample {first}")
    else:
        # Run by run: the decoder output is cut at the same frame boundaries.
        out = open(cand, "rb").read()
        at = 0
        frame_at = 0
        for index, (key, packets) in enumerate(runs):
            raw = os.path.join(tmp, "run.flac")
            open(raw, "wb").write(b"".join(packets))
            reference(raw, ref, ("-f", "flac"))
            run_rate = ffprobe_json(raw, "stream=sample_rate", ("-f", "flac"))["streams"][0]
            rates = {f["sample_rate"] for f in frames[frame_at:frame_at + len(packets)]}
            frame_at += len(packets)
            if rates != {int(run_rate["sample_rate"])}:
                notes.append(f"rate run {index}: {sorted(rates)} vs {run_rate['sample_rate']}")
            expected = open(ref, "rb").read()
            piece = os.path.join(tmp, "piece.f32")
            open(piece, "wb").write(out[at:at + len(expected)])
            at += len(expected)
            e, r, w, first = compare(piece, ref, key[1])
            if not e:
                notes.append(f"run {index} {key}: first difference at sample {first}")
            exact &= e
            rms, worst = max(rms, r), max(worst, w)
        if at != len(out):
            exact = False
            notes.append(f"{len(out) - at} bytes beyond the runs")
        notes.append(f"graded as {len(runs)} runs")
    return exact and not any(n.startswith(("layouts", "rate", "frame sample")) for n in notes), \
        rms, worst, summary, notes


def summarise(items):
    out = []
    for item in items:
        if out and out[-1][0] == item:
            out[-1][1] += 1
        else:
            out.append([item, 1])
    return ",".join(f"{k}x{n}" for k, n in out)


def grade(decoder, dirs):
    files = sorted(
        os.path.join(root, f)
        for d in dirs
        for root, _, names in os.walk(d)
        for f in names
        if f.endswith(".flac")
    )
    totals = {}
    with tempfile.TemporaryDirectory() as tmp:
        for path in files:
            ok, rms, worst, summary, notes = grade_file(decoder, path, tmp)
            group = os.path.basename(os.path.dirname(path))
            passed, total = totals.get(group, (0, 0))
            totals[group] = (passed + ok, total + 1)
            status = "PASS" if ok else "FAIL"
            print(f"{status} {os.path.relpath(path)} rms={rms:.3g} abs={worst:.3g} "
                  f"frames={summary.get('frames')} errors={summary.get('errors')} "
                  f"layout={summary.get('layout')} {'; '.join(notes)}", flush=True)
    for group, (passed, total) in totals.items():
        print(f"{group}: {passed}/{total} bit-exact")


def main(argv):
    global FFMPEG, FFPROBE
    args = [a for a in argv]
    for flag in ("--ffmpeg", "--ffprobe"):
        if flag in args:
            i = args.index(flag)
            value = args[i + 1]
            del args[i:i + 2]
            if flag == "--ffmpeg":
                FFMPEG = value
            else:
                FFPROBE = value
    if len(args) >= 2 and args[0] == "generate":
        generate(args[1])
    elif len(args) >= 3 and args[0] == "grade":
        grade(args[1], args[2:])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
