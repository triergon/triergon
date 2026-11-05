#!/usr/bin/env python3
"""Condense one evaluate.py output directory into the platform JSON kept in
research/capture/hw/ (research tooling; not a qualification verdict).

Correctness of FFmpeg-built candidates: compare.py's mux and encoder verdicts
encode Triergon's declared differences (D1-D3), which an FFmpeg candidate
cannot meet by construction (it reproduces the reference's own last-frame
loss, stream_type 0x02 and SDT provider). For those candidates the encoder
and mux are FFmpeg's unchanged mpeg1video/mpegts, so this report grades them
against the ffmpeg-sws path of the same build:
  conversion  compare.py's frozen rule (max |diff| <= 1 per plane sample)
  mux         PAT/PMT/SDT bytes identical and no TS problems
  encoder     same decoded frame count and picture types, PSNR-Y (vs own
              conversion) >= reference - 0.25 dB at <= 1.03x the bits
The Rust candidate keeps compare.py's verdicts unchanged.

  report.py <eval-dir> <platform-label> <out.json>
"""
import json
import statistics
import sys
from pathlib import Path


def ffmpeg_candidate_grade(rep, conversion):
    e, m = rep["encoder"], rep["mux"]
    c, r = e["candidate"], e["reference"]
    enc_ok = (c["decode_exit"] == 0 and not c["decode_stderr"] and c["frames"] == r["frames"]
              and e["types_equal"] and e["psnr_y_delta_db"] >= -0.25 and e["bits_ratio"] <= 1.03)
    mux_ok = (m["candidate"]["tables"] == m["reference"]["tables"] and not m["candidate"]["problems"])
    return {"conversion": conversion, "mux_tables_identical": mux_ok, "encoder_same_frames_and_quality": enc_ok,
            "frames": [c["frames"], r["frames"]], "bits_ratio": round(e["bits_ratio"], 4),
            "psnr_y_db": [round(c["mean_psnr_y"], 3), round(r["mean_psnr_y"], 3)],
            "psnr_y_delta_db": round(e["psnr_y_delta_db"], 3)}


def main():
    src, label, out = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    summary = json.loads((src / "summary.json").read_text())
    result = {"platform_label": label, "platform": summary["platform"], "rounds": summary["rounds"],
              "configs": summary["configs"], "host_loadavg": [summary.get("host_loadavg_start"), summary.get("host_loadavg_end")],
              "cases": {}}
    for cid, s in summary["cases"].items():
        rec = json.loads((src / f"{cid}.json").read_text())
        case = {"gpu_check": s["gpu_check"], "correctness": {}, "configs": {}}
        for name, verdict in (s.get("correctness") or {}).items():
            entry = {"compare_py": verdict}
            rep_path = src / f"{cid}-{name}-correctness" / "report.json"
            if name.startswith("ffmpeg") and rep_path.exists():
                entry["ffmpeg_candidate"] = ffmpeg_candidate_grade(json.loads(rep_path.read_text()), verdict.get("conversion"))
            case["correctness"][name] = entry
        for name in summary["configs"]:
            cfg = dict(s.get(name, {}))
            runs = [x for x in rec["runs"].get(name, []) if x["exit"] == 0 and "error" not in x["phases"]]
            if runs:
                sess = [x["phases"]["session_ns"] / 1e6 for x in runs]
                cfg["session_ms_min_max"] = [round(min(sess), 3), round(max(sess), 3)]
                cfg["process_wall_ms_median"] = round(statistics.median(x["process_wall_s"] * 1000 for x in runs), 3)
                cfg["gpu_device_mem_mib_max"] = max((g.get("device_mem_mib") or 0 for x in runs for g in x["gpu_samples"]), default=None)
                cfg["gpu_system_mem_in_use_max"] = max((g.get("system_mem_in_use") or 0 for x in runs for g in x["gpu_samples"]), default=None)
                cfg["heap_runs"] = rec["heap"].get(name)
                n = rec["case"]["frames"]
                # Session CPU (open..close, all threads) where the worker reports it;
                # cpu_per_frame_ms above is whole-process CPU minus the load-only
                # baseline and so includes GPU device creation and release.
                if "cpu_user_ns" in runs[0]["phases"]:
                    cfg["session_cpu_per_frame_ms"] = round(statistics.median(
                        (x["phases"]["cpu_user_ns"] + x["phases"]["cpu_sys_ns"]) / 1e6 / n for x in runs), 3)
                if runs[0]["phases"].get("footprint_peak"):
                    cfg["footprint_peak_mb_median"] = round(statistics.median(x["phases"]["footprint_peak"] for x in runs) / 1e6, 2)
                # Paired by round (each round runs every configuration once, in
                # random order): session time relative to ffmpeg-sws.
                ref = {x["round"]: x["phases"]["session_ns"] for x in rec["runs"]["ffmpeg-sws"]
                       if x["exit"] == 0 and "error" not in x["phases"]}
                ratios = sorted(x["phases"]["session_ns"] / ref[x["round"]] for x in runs if x["round"] in ref)
                if ratios:
                    cfg["session_ratio_vs_ffmpeg_sws"] = {"median": round(statistics.median(ratios), 3),
                                                          "min": round(ratios[0], 3), "max": round(ratios[-1], 3), "pairs": len(ratios)}
                rss = rec.get("rss", {}).get(name)
                if rss and rss["attributable_bytes"]:
                    cfg["attributable_rss_mb"] = round(statistics.median(rss["attributable_bytes"]) / 1e6, 2)
                    cfg["rss_pass"] = rss
            else:
                cfg["failures"] = [{"round": x["round"], "exit": x["exit"], "error": x["phases"].get("error", "")[-400:]}
                                   for x in rec["runs"].get(name, [])][:3]
                cfg["heap_runs"] = rec["heap"].get(name)
            case["configs"][name] = cfg
        result["cases"][cid] = case
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(result, indent=1) + "\n")


if __name__ == "__main__":
    main()
