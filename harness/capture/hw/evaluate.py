#!/usr/bin/env python3
"""Hardware-acceleration evaluation of the capture pipeline (platform results).

Separate from the native Linux ARM64 software qualification: no gate of
research/capture/contract.json is changed and nothing here is a
qualification verdict.

Configurations (all produce MPEG-1 video in MPEG-TS with the same encoder
settings; frames start and end in CPU memory):
  rust                  pure-Rust pipeline (harness/capture/hw/rust-phases)
  ffmpeg-sws            pinned hardware-enabled FFmpeg build, Streem's swscale path
  ffmpeg-gpu            same build, RGBA->YUV on the GPU via Vulkan (+ CPU 4:2:0)
  ffmpeg-gpu-transfer   same build, RGBA upload+download only (attribution;
                        its output is not a valid conversion and is not graded)
  ffmpeg-gpu-placebo    optional second pinned build with libplacebo: RGBA ->
                        YUV 4:2:0 entirely on the GPU (--placebo-worker)

Per case and round, every configuration runs once in random order as its own
process: per-phase wall times from the worker; process CPU (user+sys) from
wait4 minus an input-load-only baseline of the same worker; RSS sampled every
20 ms; GPU state sampled every 50 ms (macOS: system-wide IOAccelerator
statistics, Linux: nvidia-smi per-process memory and device utilisation).
Live heap comes from separate observer runs. Correctness: compare.py grades
ffmpeg-gpu and rust against the ffmpeg-sws worker.
"""
import argparse
import json
import os
import platform
import random
import re
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


def sample_rss(pid):
    if platform.system() == "Linux":
        try:
            for line in open(f"/proc/{pid}/status"):
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
        except OSError:
            return None
        return None
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) * 1024 if out else None


def sample_gpu(pid):
    """macOS only (system-wide IOAccelerator statistics); Linux uses nvidia-smi loops."""
    out = subprocess.run(["ioreg", "-r", "-c", "IOAccelerator", "-d", "1", "-w0"], capture_output=True, text=True).stdout
    util = re.search(r'"Device Utilization %"=(\d+)', out)
    inuse = re.search(r'"In use system memory"=(\d+)', out)
    return {"util": int(util.group(1)) if util else None, "system_mem_in_use": int(inuse.group(1)) if inuse else None}


def run_measured(cmd, env):
    """Run cmd, sampling RSS/GPU; returns (stdout, rusage, samples, wall)."""
    t0 = time.monotonic()
    p = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    rss, gpu, stop = [], [], threading.Event()
    linux = platform.system() == "Linux"
    loops = []
    if linux:
        # One long-running nvidia-smi per stream (a process per sample would
        # perturb a 4-vCPU host): device utilisation and per-process memory.
        loops = [subprocess.Popen(["nvidia-smi", q, "--format=csv,noheader,nounits", "-lms", "100"],
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
                 for q in ("--query-gpu=utilization.gpu,memory.used", "--query-compute-apps=pid,used_memory")]

    def sampler():
        last_gpu = 0.0
        while not stop.is_set():
            r = sample_rss(p.pid)
            if r:
                rss.append(r)
            now = time.monotonic()
            if not linux and now - last_gpu >= 0.1:
                gpu.append(sample_gpu(p.pid))
                last_gpu = now
            time.sleep(0.02)

    th = threading.Thread(target=sampler, daemon=True)
    th.start()
    out = p.stdout.read()
    err = p.stderr.read()
    _, status, usage = os.wait4(p.pid, 0)
    stop.set()
    th.join()
    if linux:
        for l in loops:
            l.terminate()
        dev, apps = (l.communicate()[0] for l in loops)
        for line in dev.strip().splitlines():
            u, m = (int(x) for x in line.split(","))
            gpu.append({"util": u, "device_mem_mib": m})
        mine = [int(m) for pid_, m in (l.split(",") for l in apps.strip().splitlines() if "," in l) if int(pid_) == p.pid]
        gpu.append({"process_mem_mib": max(mine) if mine else 0})
    return out.decode(), err.decode(), status, usage, rss, gpu, time.monotonic() - t0


def parser():
    ap = argparse.ArgumentParser()
    ap.add_argument("--hw-worker", required=True)
    ap.add_argument("--rust-worker", required=True)
    ap.add_argument("--sws-reference", required=True, help="worker graded as reference (hw build, sws path)")
    ap.add_argument("--oracle-bin", required=True)
    ap.add_argument("--observer", required=True, help="heap observer library (DYLD_INSERT_LIBRARIES / LD_PRELOAD)")
    ap.add_argument("--vk-device", required=True)
    ap.add_argument("--inputs", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--cases", nargs="+", required=True)
    ap.add_argument("--rounds", type=int, default=10)
    ap.add_argument("--extra-env", default="{}")
    ap.add_argument("--placebo-worker", help="hw_capture linked against the libplacebo-enabled build")
    ap.add_argument("--configs", nargs="+", help="subset of configurations to run")
    return ap


def configs_for(a):
    configs = {
        "rust": ([a.rust_worker], {}),
        "ffmpeg-sws": ([a.hw_worker], {"CAPTURE_CONV": "sws"}),
        "ffmpeg-gpu": ([a.hw_worker], {"CAPTURE_CONV": "vulkan", "CAPTURE_VK_DEVICE": a.vk_device}),
        "ffmpeg-gpu-transfer": ([a.hw_worker], {"CAPTURE_CONV": "vulkan-copy", "CAPTURE_VK_DEVICE": a.vk_device}),
    }
    if a.placebo_worker:
        configs["ffmpeg-gpu-placebo"] = ([a.placebo_worker], {"CAPTURE_CONV": "placebo", "CAPTURE_VK_DEVICE": a.vk_device})
    if a.configs:
        configs = {k: v for k, v in configs.items() if k in a.configs}
    return configs


def main():
    a = parser().parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    cases = {c["id"]: c for c in json.loads((ROOT / "research/capture/cases.json").read_text())["cases"]}
    base_env = dict(os.environ, **json.loads(a.extra_env))
    configs = configs_for(a)
    preload = "DYLD_INSERT_LIBRARIES" if platform.system() == "Darwin" else "LD_PRELOAD"
    rng = random.Random(20261103)
    report = {"platform": platform.uname()._asdict(), "configs": list(configs), "rounds": a.rounds, "cases": {},
              "host_loadavg_start": os.getloadavg()}
    for cid in a.cases:
        c = cases[cid]
        inp = f"{a.inputs}/{c['content']}-{c['width']}x{c['height']}-{c['frames']}.rgba"
        if not os.path.exists(inp):
            subprocess.run([sys.executable, str(ROOT / "harness/capture/generate_inputs.py"), c["content"], str(c["width"]),
                            str(c["height"]), str(c["frames"]), inp], check=True)
        geo = [str(c["width"]), str(c["height"]), str(c["flip"]), str(c["frames"]), str(c["bitrate"]), inp]
        rec = {"case": c, "runs": {k: [] for k in configs}, "load_baseline": {}, "heap": {}, "gpu_check": {},
               "host_loadavg": os.getloadavg()}
        # compare.py runs candidate and reference with one environment: pin the
        # reference to the swscale path with a wrapper.
        ref = out / "sws-reference.sh"
        ref.write_text(f'#!/bin/sh\nCAPTURE_CONV=sws exec "{a.sws_reference}" "$@"\n')
        ref.chmod(0o755)
        # Device verification: the worker must report the named GPU.
        v = subprocess.run([a.hw_worker, "phases", *geo[:4], geo[4], inp, "1"], capture_output=True, text=True,
                           env=dict(base_env, CAPTURE_CONV="vulkan", CAPTURE_VK_DEVICE=a.vk_device, CAPTURE_FFMPEG_LOG="verbose"))
        rec["gpu_check"] = {"selected": re.findall(r"Device \d+ selected: [^\n]*", v.stderr),
                            "backend": re.findall(r"Compiled using backend[^\n]*", v.stderr)[:1], "exit": v.returncode}
        for name, (cmd, env) in configs.items():
            loads = []
            for _ in range(3):
                o, e, st, u, rss, gpu, wall = run_measured(cmd + ["load", *geo], dict(base_env, **env))
                loads.append(u.ru_utime + u.ru_stime)
            rec["load_baseline"][name] = statistics.median(loads)
        for r in range(a.rounds):
            order = list(configs)
            rng.shuffle(order)
            for name in order:
                cmd, env = configs[name]
                o, e, st, u, rss, gpu, wall = run_measured(cmd + ["phases", *geo, "1"], dict(base_env, **env))
                ph = json.loads(o.strip().splitlines()[-1]) if st == 0 and o.strip() else {"error": e[-500:]}
                rec["runs"][name].append({
                    "round": r, "exit": st, "phases": ph, "process_wall_s": wall,
                    "cpu_user_s": u.ru_utime, "cpu_sys_s": u.ru_stime,
                    "cpu_minus_load_s": u.ru_utime + u.ru_stime - rec["load_baseline"][name],
                    "rss_peak_sampled": max(rss) if rss else None, "gpu_samples": gpu})
        for name, (cmd, env) in configs.items():
            heaps = []
            for k in range(3):
                rpt = out / f"{cid}-{name}-heap-{k}.json"
                rpt.unlink(missing_ok=True)
                o = subprocess.run(cmd + ["memory", *geo], capture_output=True, text=True,
                                   env=dict(base_env, **env, **{preload: a.observer, "SAFE_MEDIA_ALLOCATION_REPORT": str(rpt),
                                                                "SAFE_MEDIA_MEMORY_MODE": "heap"}))
                d = json.loads(rpt.read_text()) if rpt.exists() else {"status": "missing", "stderr": o.stderr[-300:]}
                tail = o.stdout.strip().splitlines()[-1] if o.stdout.strip() else "{}"
                heaps.append({"status": d.get("status"), "heap_peak_bytes": d.get("heap_peak_bytes"),
                              "result": json.loads(tail) if tail.startswith("{") else tail})
            rec["heap"][name] = heaps
        graded = [(n, configs[n][0][0], configs[n][1]) for n in ("ffmpeg-gpu", "ffmpeg-gpu-placebo") if n in configs]
        if "rust" in configs:
            graded.append(("rust-vs-ffmpeg-sws", ROOT / "target/release/capture-worker", {}))
        for name, worker, env in graded:
            r = subprocess.run([sys.executable, str(ROOT / "harness/capture/compare.py"), "--candidate", str(worker),
                                "--reference", str(ref), "--ffmpeg-bin", a.oracle_bin, "--case", json.dumps(c),
                                "--input", inp, "--out", str(out / f"{cid}-{name}-correctness")],
                               capture_output=True, text=True, env=dict(base_env, **{**env, "CAPTURE_CONV": env.get("CAPTURE_CONV", "sws")}))
            for f in (out / f"{cid}-{name}-correctness").glob("*.yuv"):
                f.unlink()
            rec.setdefault("correctness", {})[name] = json.loads(r.stdout) if r.returncode == 0 else {"error": r.stderr[-800:]}
        (out / f"{cid}.json").write_text(json.dumps(rec, indent=1))
        report["cases"][cid] = summarize(rec)
        print(json.dumps({cid: report["cases"][cid]}), flush=True)
    report["host_loadavg_end"] = os.getloadavg()
    (out / "summary.json").write_text(json.dumps(report, indent=1))
    return 0


def summarize(rec):
    s = {"gpu_check": rec["gpu_check"], "correctness": rec.get("correctness")}
    for name, runs in rec["runs"].items():
        ok = [x for x in runs if x["exit"] == 0 and "error" not in x["phases"]]
        if not ok:
            s[name] = {"error": runs[0]["phases"] if runs else "no runs"}
            continue
        med = lambda f: statistics.median(f(x) for x in ok)
        n_frames = rec["case"]["frames"]
        gpu_util = [g.get("util") for x in ok for g in x["gpu_samples"] if g.get("util") is not None]
        s[name] = {
            "runs": len(ok),
            "session_ms": med(lambda x: x["phases"]["session_ns"] / 1e6),
            "startup_device_ms": med(lambda x: x["phases"]["startup_device_ns"] / 1e6),
            "open_ms": med(lambda x: x["phases"]["open_ns"] / 1e6),
            "frames_ms": med(lambda x: x["phases"]["frames_ns"] / 1e6),
            "throughput_fps": med(lambda x: n_frames / (x["phases"]["frames_ns"] / 1e9)),
            "frame_latency_median_ms": med(lambda x: x["phases"]["frame"]["median"] / 1e6),
            "frame_latency_p95_ms": med(lambda x: x["phases"]["frame"]["p95"] / 1e6),
            "convert_median_ms": med(lambda x: x["phases"]["convert"]["median"] / 1e6),
            "drain_ms": med(lambda x: x["phases"]["drain_ns"] / 1e6),
            "reset_ms": med(lambda x: x["phases"]["reset_ns"] / 1e6),
            "close_ms": med(lambda x: x["phases"]["close_ns"] / 1e6),
            "shutdown_device_ms": med(lambda x: x["phases"]["shutdown_device_ns"] / 1e6),
            "cpu_s_minus_load": med(lambda x: x["cpu_minus_load_s"]),
            "cpu_per_frame_ms": med(lambda x: x["cpu_minus_load_s"] * 1000 / n_frames),
            "rss_peak_mb": med(lambda x: (x["rss_peak_sampled"] or 0) / 1e6),
            "footprint_peak_mb": med(lambda x: x["phases"].get("footprint_peak", 0) / 1e6),
            "gpu_util_median": statistics.median(gpu_util) if gpu_util else None,
            "gpu_util_max": max(gpu_util) if gpu_util else None,
            "gpu_process_mem_mib_max": max((g.get("process_mem_mib") or 0 for x in ok for g in x["gpu_samples"]), default=None),
            "heap_peak_mb": [round((h["heap_peak_bytes"] or 0) / 1e6, 2) for h in rec["heap"].get(name, [])],
            "heap_status": sorted({h["status"] for h in rec["heap"].get(name, [])}),
            "output_hashes": sorted({x["phases"]["fnv1a64"] for x in ok}),
        }
    return s


if __name__ == "__main__":
    sys.exit(main())
