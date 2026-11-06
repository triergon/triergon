#!/usr/bin/env python3
"""Capture-lane correctness comparison (research tooling).

Runs one required case through the Rust worker (candidate) and the FFmpeg
reference worker, then applies the predeclared rules of
research/capture/contract.json `comparison_rules`:

  conversion  every Y/U/V sample within +-1 of the reference conversion
  mux         own TS checker on both outputs (sync, continuity, PCR = DTS*300
              on each PES start, PTS/DTS per frame, table cadence, PSI bytes)
              plus FFmpeg re-demux; PES payload == elementary stream
  encoder     error-free independent FFmpeg decode of every frame, identical
              picture types, VBV underflow no worse than the reference, and
              (rule v2, contract.json) PSNR-Y >= ref - 0.25 dB where the
              candidate uses <= 1.03x the reference bits or <= 1.03x the
              requested budget; if the reference spent more, the candidate is
              re-run at higher requested rates and its PSNR interpolated
              (log-linear in bits) at the reference's bits. The v1 verdict
              (<= 1.03x reference bits, no budget/interpolation clause) is kept
              alongside in every report.
  pipeline    all of the above on one complete session

Outputs one JSON report; every observation is kept, failures included.
"""
import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import sys

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from harness.common import digest as sha256  # noqa: E402

PACKET = 188


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw)


def planes(data, w, h, frames):
    size = w * h * 3 // 2
    out = []
    for i in range(frames):
        f = np.frombuffer(data, dtype=np.uint8, count=size, offset=i * size)
        y = f[: w * h].reshape(h, w)
        u = f[w * h: w * h + w * h // 4].reshape(h // 2, w // 2)
        v = f[w * h + w * h // 4:].reshape(h // 2, w // 2)
        out.append((y, u, v))
    return out


def psnr(a, b):
    mse = np.mean((a.astype(np.int32) - b.astype(np.int32)) ** 2)
    return float("inf") if mse == 0 else 10 * math.log10(255 * 255 / mse)


def parse_ts(data):
    """Independent TS walk: tables, PES, PCR, continuity; returns facts."""
    facts = {"errors": [], "pes": [], "tables": [], "packets": len(data) // PACKET}
    if len(data) % PACKET:
        facts["errors"].append("length not a multiple of 188")
    cc = {}
    current = None
    for i in range(len(data) // PACKET):
        p = data[i * PACKET:(i + 1) * PACKET]
        if p[0] != 0x47:
            facts["errors"].append(f"sync {i}")
            continue
        pusi = p[1] >> 6 & 1
        pid = (p[1] & 0x1F) << 8 | p[2]
        afc = p[3] >> 4 & 3
        c = p[3] & 15
        if afc & 1:
            if pid in cc and (cc[pid] + 1) & 15 != c:
                facts["errors"].append(f"continuity pid {pid:#x} packet {i}")
            cc[pid] = c
        off = 4
        pcr = None
        rai = False
        if afc & 2:
            length = p[4]
            off = 5 + length
            if length:
                flags = p[5]
                rai = bool(flags & 0x40)
                if flags & 0x10:
                    b = p[6:12]
                    base = b[0] << 25 | b[1] << 17 | b[2] << 9 | b[3] << 1 | b[4] >> 7
                    pcr = base * 300 + ((b[4] & 1) << 8 | b[5])
        payload = p[off:]
        if pid in (0x0000, 0x0011, 0x1000):
            if pusi:
                ptr = payload[0]
                sec = payload[1 + ptr:]
                length = (sec[1] & 0x0F) << 8 | sec[2]
                facts["tables"].append({"packet": i, "pid": pid, "section": sec[:3 + length].hex(),
                                        "pes_index": len(facts["pes"])})
            continue
        if pusi:
            if payload[:3] != b"\x00\x00\x01":
                facts["errors"].append(f"pes start code {i}")
                continue
            flags = payload[7]
            hlen = payload[8]

            def ts(b):
                return ((b[0] >> 1) & 7) << 30 | (b[1] << 7 | b[2] >> 1) << 15 | (b[3] << 7 | b[4] >> 1)

            pts = ts(payload[9:14]) if flags & 0x80 else None
            dts = ts(payload[14:19]) if flags & 0x40 else pts
            current = {"pid": pid, "pts": pts, "dts": dts, "pcr": pcr, "rai": rai,
                       "stream_id": payload[3], "length": payload[4] << 8 | payload[5], "data": bytearray()}
            facts["pes"].append(current)
            current["data"] += payload[9 + hlen:]
        elif current is not None and pid == current["pid"]:
            if pcr is not None:
                facts["errors"].append(f"pcr inside pes {i}")
            current["data"] += payload
    return facts


def picture_type(es):
    """picture_coding_type of the first picture header in an elementary stream."""
    i = es.find(b"\x00\x00\x01\x00")
    return None if i < 0 or i + 6 > len(es) else (es[i + 5] >> 3) & 7


def check_mux(facts):
    """Contract `mux` cadence/timing rules on one TS. Key pictures are read
    from each picture header (I = 1), so scene-change I pictures count."""
    problems = list(facts["errors"])
    last_pat = last_sdt = None
    prev_key = False
    seen = {}
    for t in facts["tables"]:
        seen.setdefault(t["pes_index"], []).append(t["pid"])
    for k, pes in enumerate(facts["pes"]):
        dts, pts, pcr = pes["dts"], pes["pts"], pes["pcr"]
        if dts != 3000 * k or pts != 3000 * (k + 1):
            problems.append(f"frame {k}: pts {pts} dts {dts}")
        if pcr != dts * 300:
            problems.append(f"frame {k}: pcr {pcr}")
        key = picture_type(bytes(pes["data"])) == 1
        if pes["rai"] != key:
            problems.append(f"frame {k}: random access {pes['rai']}")
        if pes["stream_id"] != 0xE0 or pes["length"] != 0:
            problems.append(f"frame {k}: pes header")
        want = []
        if last_sdt is None or pcr - last_sdt >= 13_500_000:
            want.append(0x11)
            last_sdt = pcr if last_sdt is None else max(last_sdt, pcr)
        if (key and not prev_key) or last_pat is None or pcr - last_pat >= 2_700_000:
            want += [0x00, 0x1000]
            last_pat = pcr if last_pat is None else max(last_pat, pcr)
        if seen.get(k, []) != want:
            problems.append(f"frame {k}: tables {seen.get(k)} want {want}")
        prev_key = key
    return problems


def vbv(sizes_bits, max_rate, buffer_bits, fps=30.0):
    """Variable-rate VBV (Annex C): returns total underflow bits."""
    fullness = buffer_bits
    under = 0.0
    for b in sizes_bits:
        if b > fullness:
            under += b - fullness
            fullness = 0
        else:
            fullness -= b
        fullness = min(buffer_bits, fullness + max_rate / fps)
    return under


def ffprobe_packets(ffprobe, path):
    out = run([ffprobe, "-v", "error", "-show_packets", "-show_entries", "packet=pts,dts,size,flags",
               "-of", "json", path]).stdout
    return json.loads(out)["packets"]


def decode(ffmpeg, path, out):
    r = subprocess.run([ffmpeg, "-v", "error", "-err_detect", "+crccheck+bitstream+buffer+explode",
                        "-i", path, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-y", out],
                       capture_output=True, text=True)
    return r.returncode, r.stderr


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--candidate", required=True)
    ap.add_argument("--reference", required=True)
    ap.add_argument("--ffmpeg-bin", required=True, help="directory with ffmpeg and ffprobe used as independent decoder")
    ap.add_argument("--case", required=True, help="JSON case object or path")
    ap.add_argument("--input", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    case = json.load(open(a.case)) if os.path.exists(a.case) else json.loads(a.case)
    w, h, flip, n, rate = case["width"], case["height"], case["flip"], case["frames"], case["bitrate"]
    os.makedirs(a.out, exist_ok=True)
    ffmpeg = os.path.join(a.ffmpeg_bin, "ffmpeg")
    ffprobe = os.path.join(a.ffmpeg_bin, "ffprobe")
    report = {"case": case, "input_sha256": sha256(a.input),
              "candidate_sha256": sha256(a.candidate), "reference_sha256": sha256(a.reference)}
    paths = {}
    for role, worker in (("candidate", a.candidate), ("reference", a.reference)):
        yuv = os.path.join(a.out, f"{role}.convert.yuv")
        run([worker, "convert", str(w), str(h), str(flip), str(n), a.input, yuv])
        ts = os.path.join(a.out, f"{role}.ts")
        result = run([worker, "pipeline", str(w), str(h), str(flip), str(n), str(rate), a.input, ts]).stdout
        paths[role] = {"yuv": yuv, "ts": ts, "worker": json.loads(result)}
    # Conversion rule.
    cand = open(paths["candidate"]["yuv"], "rb").read()
    ref = open(paths["reference"]["yuv"], "rb").read()
    conv = {"bytes_equal": cand == ref}
    if len(cand) != len(ref):
        conv["error"] = "size"
        conv["pass"] = False
    else:
        d = np.abs(np.frombuffer(cand, np.uint8).astype(np.int16) - np.frombuffer(ref, np.uint8).astype(np.int16))
        conv.update(max_abs=int(d.max()), exact_fraction=float(np.mean(d == 0)), pass_=bool(d.max() <= 1))
        conv["pass"] = conv.pop("pass_")
    report["conversion"] = conv
    # Mux rule.
    mux = {}
    for role in ("candidate", "reference"):
        data = open(paths[role]["ts"], "rb").read()
        facts = parse_ts(data)
        problems = check_mux(facts)
        mux[role] = {"packets": facts["packets"], "pictures": len(facts["pes"]), "problems": problems[:50],
                     "problem_count": len(problems),
                     "tables": sorted({(t["pid"], t["section"]) for t in facts["tables"]})}
        paths[role]["facts"] = facts
    rt = {pid: s for pid, s in mux["reference"]["tables"]}
    ct = {pid: s for pid, s in mux["candidate"]["tables"]}
    mux["pat_equal"] = rt.get(0) == ct.get(0)
    pmt_r, pmt_c = rt.get(0x1000, ""), ct.get(0x1000, "")
    mux["pmt_equal_except_stream_type_D2"] = (pmt_r[:24] + pmt_r[26:-8]) == (pmt_c[:24] + pmt_c[26:-8])
    mux["pmt_stream_types"] = {"reference": pmt_r[24:26], "candidate": pmt_c[24:26]}
    mux["sdt_names"] = {"reference": rt.get(0x11, "")[38:-8], "candidate": ct.get(0x11, "")[38:-8]}
    for t in ("candidate", "reference"):
        mux[t]["tables"] = [(hex(p), s) for p, s in mux[t]["tables"]]
    # Re-demux with FFmpeg.
    for role in ("candidate", "reference"):
        pk = ffprobe_packets(ffprobe, paths[role]["ts"])
        paths[role]["probe"] = pk
        mux[role]["ffprobe_packets"] = len(pk)
        mux[role]["ffprobe_timing_ok"] = all(int(p["dts"]) == 3000 * k and int(p["pts"]) == 3000 * (k + 1)
                                             for k, p in enumerate(pk))
    mux["pass"] = (mux["candidate"]["problem_count"] == 0 and mux["pat_equal"]
                   and mux["pmt_equal_except_stream_type_D2"] and mux["candidate"]["ffprobe_timing_ok"]
                   and mux["candidate"]["ffprobe_packets"] == n)
    report["mux"] = mux
    # Encoder rule.
    enc = {}
    for role in ("candidate", "reference"):
        out = os.path.join(a.out, f"{role}.decoded.yuv")
        code, err = decode(ffmpeg, paths[role]["ts"], out)
        frames = os.path.getsize(out) // (w * h * 3 // 2) if os.path.exists(out) else 0
        sizes = [len(p["data"]) * 8 for p in paths[role]["facts"]["pes"]]
        types = "".join({1: "I", 2: "P"}.get(picture_type(bytes(p["data"])), "?") for p in paths[role]["facts"]["pes"])
        enc[role] = {"decode_exit": code, "decode_stderr": err[-2000:], "frames": frames,
                     "types": types, "es_bits": sizes, "decoded": out}
    common = min(enc["candidate"]["frames"], enc["reference"]["frames"])
    src_ref = planes(ref, w, h, n)
    src_cand = planes(cand, w, h, n)
    for role in ("candidate", "reference"):
        dec = planes(open(enc[role]["decoded"], "rb").read(), w, h, enc[role]["frames"])
        own = src_cand if role == "candidate" else src_ref
        enc[role]["psnr_y_vs_reference_conversion"] = [psnr(dec[i][0], src_ref[i][0]) for i in range(common)]
        enc[role]["psnr_y_vs_own_conversion"] = [psnr(dec[i][0], own[i][0]) for i in range(common)]
        enc[role]["psnr_u_vs_own_conversion"] = [psnr(dec[i][1], own[i][1]) for i in range(common)]
        enc[role]["psnr_v_vs_own_conversion"] = [psnr(dec[i][2], own[i][2]) for i in range(common)]
        mse = np.mean([10 ** (-p / 10) for p in enc[role]["psnr_y_vs_own_conversion"] if p != float("inf")] or [0])
        enc[role]["mean_psnr_y"] = float("inf") if mse == 0 else -10 * math.log10(mse)
        enc[role]["common_bits"] = sum(enc[role]["es_bits"][:common])
        enc[role]["vbv_underflow_bits"] = vbv(enc[role]["es_bits"], rate * 7 // 5, (rate // 7 + 16383) // 16384 * 16384)
        del enc[role]["decoded"]
    c, r = enc["candidate"], enc["reference"]
    enc["common_frames"] = common
    enc["bits_ratio"] = c["common_bits"] / r["common_bits"]
    enc["psnr_y_delta_db"] = c["mean_psnr_y"] - r["mean_psnr_y"]
    enc["types_equal"] = c["types"][:common] == r["types"][:common]
    base_ok = (c["decode_exit"] == 0 and c["frames"] == n and not c["decode_stderr"]
               and enc["types_equal"] and c["vbv_underflow_bits"] <= r["vbv_underflow_bits"])
    enc["pass_v1"] = base_ok and enc["psnr_y_delta_db"] >= -0.25 and enc["bits_ratio"] <= 1.03
    budget = rate * common / 30.0
    cb, rb = c["common_bits"], r["common_bits"]
    if cb <= 1.03 * rb:
        mode, qpass = "equal-or-fewer-bits", enc["psnr_y_delta_db"] >= -0.25
    elif cb <= 1.03 * budget:
        mode, qpass = "within-requested-budget", enc["psnr_y_delta_db"] >= -0.25
    else:
        mode, qpass = "over-budget", False
    points = [(cb, c["mean_psnr_y"], rate)]
    interpolated = None
    if not qpass and rb > 1.03 * cb:
        for factor in (1.25, 1.6, 2.0, 2.6, 3.4, 4.5):
            extra = int(rate * factor)
            ts = os.path.join(a.out, f"candidate.rate{extra}.ts")
            run([a.candidate, "pipeline", str(w), str(h), str(flip), str(n), str(extra), a.input, ts])
            out = ts + ".yuv"
            decode(ffmpeg, ts, out)
            dec = planes(open(out, "rb").read(), w, h, common)
            mse = np.mean([10 ** (-psnr(dec[i][0], src_cand[i][0]) / 10) for i in range(common)])
            bits = sum(len(p["data"]) * 8 for p in parse_ts(open(ts, "rb").read())["pes"][:common])
            points.append((bits, -10 * math.log10(mse), extra))
            os.remove(out)
            if bits >= rb:
                break
        ordered = sorted(points)
        for (b0, p0, _), (b1, p1, _) in zip(ordered, ordered[1:]):
            if b0 <= rb <= b1 and b1 > b0:
                t = (math.log(rb) - math.log(b0)) / (math.log(b1) - math.log(b0))
                interpolated = p0 + t * (p1 - p0)
        if interpolated is not None:
            mode, qpass = "equal-bits-interpolated", interpolated >= r["mean_psnr_y"] - 0.25
        else:
            mode = "reference-bits-unreachable"
    enc["v2"] = {"mode": mode, "quality_pass": qpass, "budget_bits": budget, "points": points,
                 "interpolated_psnr_y": interpolated,
                 "reference_exceeds_request": r["vbv_underflow_bits"] > 0 or rb > 1.03 * budget}
    enc["pass"] = base_ok and qpass
    report["encoder"] = enc
    report["pipeline_pass"] = conv["pass"] and mux["pass"] and enc["pass"]
    with open(os.path.join(a.out, "report.json"), "w") as f:
        json.dump(report, f, indent=1)
    summary = {"conversion": conv["pass"], "conversion_max_abs": conv.get("max_abs"),
               "conversion_exact": round(conv.get("exact_fraction", 0), 5), "mux": mux["pass"],
               "encoder": enc["pass"], "encoder_v1": enc["pass_v1"], "encoder_mode": enc["v2"]["mode"],
               "interpolated_psnr_y": enc["v2"]["interpolated_psnr_y"] and round(enc["v2"]["interpolated_psnr_y"], 3),
               "reference_exceeds_request": enc["v2"]["reference_exceeds_request"],
               "bits_ratio": round(enc["bits_ratio"], 4),
               "psnr_y": [round(c["mean_psnr_y"], 3), round(r["mean_psnr_y"], 3)],
               "psnr_delta": round(enc["psnr_y_delta_db"], 3),
               "vbv_underflow": [c["vbv_underflow_bits"], r["vbv_underflow_bits"]],
               "pipeline": report["pipeline_pass"]}
    print(json.dumps(summary))
    return 0


if __name__ == "__main__":
    sys.exit(main())
