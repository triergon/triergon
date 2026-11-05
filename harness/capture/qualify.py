#!/usr/bin/env python3
"""D5/O62 capture qualification on native Linux (research tooling).

For every required case in research/capture/cases.json:
  1. generate the RGBA input and record its SHA-256;
  2. correctness: harness/capture/compare.py (conversion, mux, encoder,
     pipeline rules of research/capture/contract.json);
  3. runtime: paired, randomly ordered worker processes; each sample is the
     median of `--iterations` in-process sessions after one discarded warm-up
     session; the ratio interval is the shared harness's bootstrap of the
     median paired ratio (harness.runner.ratio_interval). Gate: upper 95%
     bound <= 1.25 (target 0.80);
  4. memory: the shared glibc observer (harness/allocations-linux.c) in heap
     and rss mode, separate runs, alternating order. Gates: candidate heap
     peak strictly below the reference in every pair, and the approved
     conservative RSS formula (candidate VmPeak ceiling vs reference observed
     peak) with upper 95% ratio strictly below 1.0.

The machine must be otherwise idle; a CPU-contention check runs before every
timing/memory batch (harness.runner.competing_builds) and contaminated
batches are retried, never silently used. Every raw observation is written.
"""
import argparse
import hashlib
import json
import os
import platform
import random
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from harness.runner import competing_builds, ratio_interval  # noqa: E402

RUNTIME_GATE = 1.25
RUNTIME_TARGET = 0.80


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def wait_idle(log):
    for attempt in range(20):
        busy = competing_builds()
        if not busy:
            return attempt
        log.append({"contaminants": busy, "utc": time.time()})
        time.sleep(3)
    raise SystemExit(f"machine not idle: {log[-1]}")


def time_sample(worker, case, path, iterations):
    args = [worker, "time", str(case["width"]), str(case["height"]), str(case["flip"]), str(case["frames"]),
            str(case["bitrate"]), path, str(iterations + 1)]
    out = subprocess.run(args, check=True, capture_output=True, text=True).stdout.splitlines()
    rows = [json.loads(line) for line in out]
    measured = [r["ns"] for r in rows[1:]]
    hashes = sorted({r["fnv1a64"] for r in rows})
    return {"warmup_ns": rows[0]["ns"], "ns": measured, "median_ns": statistics.median(measured), "hashes": hashes,
            "bytes": rows[0]["bytes"]}


def runtime(cand, ref, case, path, pairs, iterations, seed):
    rng = random.Random(seed)
    contamination = []
    samples = []
    for i in range(pairs):
        wait_idle(contamination)
        order = ["candidate", "reference"]
        rng.shuffle(order)
        pair = {"index": i, "order": order}
        for role in order:
            pair[role] = time_sample(cand if role == "candidate" else ref, case, path, iterations)
        samples.append(pair)
    interval = ratio_interval([(p["candidate"]["median_ns"], p["reference"]["median_ns"]) for p in samples])
    stable = all(len(p[r]["hashes"]) == 1 for p in samples for r in ("candidate", "reference"))
    return {"pairs": pairs, "iterations": iterations, "seed": seed, "samples": samples, "ratio": interval,
            "candidate_median_ms": statistics.median(p["candidate"]["median_ns"] for p in samples) / 1e6,
            "reference_median_ms": statistics.median(p["reference"]["median_ns"] for p in samples) / 1e6,
            "deterministic_output": stable, "contamination_waits": contamination,
            "gate_pass": interval["upper_95"] <= RUNTIME_GATE, "target_pass": interval["upper_95"] <= RUNTIME_TARGET}


def observe(worker, case, path, observer, mode, receipt):
    env = dict(os.environ, LD_PRELOAD=observer, SAFE_MEDIA_ALLOCATION_REPORT=receipt, SAFE_MEDIA_MEMORY_MODE=mode)
    args = [worker, "memory", str(case["width"]), str(case["height"]), str(case["flip"]), str(case["frames"]),
            str(case["bitrate"]), path]
    r = subprocess.run(args, env=env, capture_output=True, text=True)
    data = json.loads(Path(receipt).read_text()) if Path(receipt).exists() else None
    return {"exit": r.returncode, "stdout": r.stdout[-400:], "stderr": r.stderr[-400:], "receipt": data}


def memory(cand, ref, case, path, observer, pairs, out, seed):
    rng = random.Random(seed)
    result = {"pairs": pairs, "heap": [], "rss": [], "contamination_waits": []}
    for mode in ("heap", "rss"):
        for i in range(pairs):
            wait_idle(result["contamination_waits"])
            order = ["candidate", "reference"]
            rng.shuffle(order)
            pair = {"index": i, "order": order}
            for role in order:
                receipt = out / f"{mode}-{i}-{role}.json"
                if receipt.exists():
                    receipt.unlink()
                pair[role] = observe(cand if role == "candidate" else ref, case, path, observer, mode, str(receipt))
            result[mode].append(pair)

    def valid(o, mode):
        d = o["receipt"]
        return o["exit"] == 0 and d is not None and d["status"] == "measured" and d["mode"] == mode and d["error_bits"] == 0

    heap_ok = all(valid(p[r], "heap") for p in result["heap"] for r in ("candidate", "reference"))
    rss_ok = all(valid(p[r], "rss") for p in result["rss"] for r in ("candidate", "reference"))
    result["heap_valid"], result["rss_valid"] = heap_ok, rss_ok
    if heap_ok:
        c = [p["candidate"]["receipt"]["heap_peak_bytes"] for p in result["heap"]]
        r = [p["reference"]["receipt"]["heap_peak_bytes"] for p in result["heap"]]
        result["heap_peak_bytes"] = {"candidate": c, "reference": r}
        result["heap_ratio"] = ratio_interval(list(zip(c, r)))
        result["heap_gate_pass"] = all(a < b for a, b in zip(c, r)) and result["heap_ratio"]["upper_95"] < 1.0
    else:
        result["heap_gate_pass"] = False
    if rss_ok:
        pairs_bound, pairs_observed = [], []
        for p in result["rss"]:
            cd, rd = p["candidate"]["receipt"], p["reference"]["receipt"]
            page = cd["page_size"]
            cand_upper = cd["rss_peak_upper_bound_bytes"] - cd["rss_start"] + 2 * page
            ref_lower = rd["rss_peak_observed"] - rd["rss_start"] - 2 * page
            pairs_bound.append((cand_upper, ref_lower))
            pairs_observed.append((cd["rss_peak_observed"] - cd["rss_start"] + 1, rd["rss_peak_observed"] - rd["rss_start"] + 1))
        result["rss_bound_pairs"] = pairs_bound
        result["rss_observed_growth_pairs"] = pairs_observed
        if all(a > 0 and b > 0 for a, b in pairs_bound):
            result["rss_bound_ratio"] = ratio_interval(pairs_bound)
            result["rss_gate_pass"] = result["rss_bound_ratio"]["upper_95"] < 1.0
        else:
            result["rss_bound_ratio"] = None
            result["rss_gate_pass"] = False
        result["rss_observed_ratio"] = ratio_interval(pairs_observed)
    else:
        result["rss_gate_pass"] = False
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--candidate", required=True)
    ap.add_argument("--reference", required=True, help="gating reference worker (best allowed FFmpeg build)")
    ap.add_argument("--reference-streem", help="exact Streem-configuration worker, reported separately")
    ap.add_argument("--oracle-bin", required=True)
    ap.add_argument("--observer", required=True)
    ap.add_argument("--cases", default=str(ROOT / "research/capture/cases.json"))
    ap.add_argument("--only", nargs="*")
    ap.add_argument("--inputs", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--pairs", type=int, default=30)
    ap.add_argument("--iterations", type=int, default=3)
    ap.add_argument("--memory-pairs", type=int, default=10)
    ap.add_argument("--seed", type=int, default=20261102)
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    cases = json.loads(Path(a.cases).read_text())["cases"]
    if a.only:
        cases = [c for c in cases if c["id"] in a.only]
    manifest = {}
    for top in ("crates/capture", "harness/capture", "research/capture"):
        for f in sorted((ROOT / top).rglob("*")):
            if f.is_file() and "__pycache__" not in f.parts:
                manifest[str(f.relative_to(ROOT))] = sha256(f)
    for f in ("Cargo.toml", "Cargo.lock", "harness/runner.py", "harness/allocations-linux.c", "harness/rss-bound-api.h"):
        manifest[f] = sha256(ROOT / f)
    source_digest = hashlib.sha256(json.dumps(manifest, sort_keys=True).encode()).hexdigest()
    identity = {"source_digest": source_digest, "source_manifest": manifest,
                "candidate_sha256": sha256(a.candidate), "reference_sha256": sha256(a.reference),
                "reference_streem_sha256": sha256(a.reference_streem) if a.reference_streem else None,
                "observer_sha256": sha256(a.observer), "cases_sha256": sha256(a.cases),
                "host": platform.uname()._asdict(), "utc": time.time()}
    (out / "identity.json").write_text(json.dumps(identity, indent=1))
    summary = []
    for case in cases:
        cdir = out / case["id"]
        cdir.mkdir(exist_ok=True)
        path = Path(a.inputs) / f"{case['content']}-{case['width']}x{case['height']}-{case['frames']}.rgba"
        if not path.exists():
            subprocess.run([sys.executable, str(ROOT / "harness/capture/generate_inputs.py"), case["content"],
                            str(case["width"]), str(case["height"]), str(case["frames"]), str(path)], check=True)
        record = {"case": case, "input": str(path), "input_sha256": sha256(path)}
        cmp = subprocess.run([sys.executable, str(ROOT / "harness/capture/compare.py"), "--candidate", a.candidate,
                              "--reference", a.reference, "--ffmpeg-bin", a.oracle_bin, "--case", json.dumps(case),
                              "--input", str(path), "--out", str(cdir / "correctness")], capture_output=True, text=True)
        record["correctness_exit"] = cmp.returncode
        record["correctness_summary"] = json.loads(cmp.stdout) if cmp.returncode == 0 else {"error": cmp.stderr[-2000:]}
        record["runtime"] = runtime(a.candidate, a.reference, case, str(path), a.pairs, a.iterations, a.seed)
        if a.reference_streem:
            record["runtime_vs_streem_config"] = runtime(a.candidate, a.reference_streem, case, str(path),
                                                         max(10, a.pairs // 3), a.iterations, a.seed + 1)
        mdir = cdir / "memory"
        mdir.mkdir(exist_ok=True)
        record["memory"] = memory(a.candidate, a.reference, case, str(path), a.observer, a.memory_pairs, mdir, a.seed + 2)
        c = record["correctness_summary"]
        record["verdict"] = {
            "conversion": bool(c.get("conversion")), "mux": bool(c.get("mux")), "encoder": bool(c.get("encoder")),
            "pipeline_parity": bool(c.get("pipeline")),
            "runtime_gate": record["runtime"]["gate_pass"], "runtime_target_0_80": record["runtime"]["target_pass"],
            "heap_gate": record["memory"]["heap_gate_pass"], "rss_gate": record["memory"]["rss_gate_pass"],
        }
        record["verdict"]["all_gates"] = all(record["verdict"][k] for k in
                                             ("pipeline_parity", "runtime_gate", "heap_gate", "rss_gate"))
        (cdir / "result.json").write_text(json.dumps(record, indent=1))
        line = {"id": case["id"], **record["verdict"], "runtime_upper95": round(record["runtime"]["ratio"]["upper_95"], 4),
                "correctness": c}
        summary.append(line)
        print(json.dumps(line), flush=True)
    (out / "summary.json").write_text(json.dumps(summary, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
