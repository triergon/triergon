"""Operation-scoped macOS memory measurements with conservative RSS bounds.

The native observer is external tooling. Heap and RSS are separate instrumented
runs; timing workers never load it. Invalid/unknown coverage remains a non-pass.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def observe(worker, operation, path, mode):
    library = ROOT / "target/harness/allocations.dylib"
    if platform.system() != "Darwin" or not library.exists():
        return {"status": "unsupported-platform"}
    with tempfile.TemporaryDirectory(prefix="safe-media-memory-") as directory:
        receipt = Path(directory) / "window.json"
        env = dict(os.environ, DYLD_INSERT_LIBRARIES=str(library), SAFE_MEDIA_ALLOCATION_REPORT=str(receipt), SAFE_MEDIA_MEMORY_MODE=mode)
        try:
            argv = [str(worker)] if operation is None else [str(worker), operation, str(path)]
            result = subprocess.run(argv, capture_output=True, timeout=30, env=env)
            if result.returncode or not receipt.exists():
                return {"status": "observer-failed", "stderr": result.stderr.decode(errors="replace")[-2048:]}
            data = json.loads(receipt.read_text())
            output = json.loads(result.stdout.decode().splitlines()[-1])
            if data.get("schema") != 2 or data.get("status") != "measured" or data["mode"] != mode:
                return {"status": "observer-invalid", "receipt": data}
            return {"status": "measured", "observer": data, "work": output, "instrument_sha256": sha(library)}
        except (OSError, ValueError, IndexError, subprocess.TimeoutExpired) as error:
            return {"status": "observer-failed", "error": str(error)}


def qualification(candidate, reference):
    probe = ROOT / "target/harness/memory-check"
    heap, rss = (observe(probe, None, None, mode) for mode in ("heap", "rss"))
    errors = []
    if heap["status"] != "measured" or heap["observer"]["heap_peak_bytes"] != heap["work"].get("expected_peak_bytes") or heap["observer"]["heap_live_end"] != heap["observer"]["heap_live_start"]:
        errors.append("known-allocation peak or release mismatch")
    if rss["status"] != "measured" or rss["observer"]["rss_peak_observed"] <= rss["observer"]["rss_start"]:
        errors.append("known-allocation resident growth not observed")
    # This certifies the selected workers' allocation entry points, not every
    # possible system-library path or a universal malloc implementation.
    symbols = {}
    allowed = {"malloc", "calloc", "realloc", "free", "aligned_alloc", "posix_memalign"}
    for name, worker in (("candidate", candidate), ("reference", reference)):
        undefined = subprocess.check_output(["nm", "-u", worker], text=True)
        allocation = {s.lstrip("_") for s in re.findall(r"^\s*(_\w+)\s*$", undefined, re.M) if re.search(r"alloc|free$|strdup|strndup|asprintf", s)}
        symbols[name] = sorted(allocation)
        if allocation - allowed:
            errors.append(name + ": uncovered allocator imports " + str(sorted(allocation - allowed)))
    return {"status": "qualified-selected-workers" if not errors else "unqualified", "errors": errors,
            "probe_heap": heap, "probe_rss": rss, "allocator_imports": symbols,
            "probe_sha256": sha(probe), "observer_sha256": sha(ROOT / "target/harness/allocations.dylib"),
            "worker_hashes": {"candidate": sha(candidate), "reference": sha(reference)}}


def measure(candidate, reference, path, pairs, seed, certificate, interval):
    if certificate["status"] != "qualified-selected-workers":
        return {"status": "observer-unqualified", "certificate": certificate}
    rng = random.Random(seed)
    samples = []
    for _ in range(pairs):
        row = {}
        jobs = [(name, mode) for name in ("candidate", "reference") for mode in ("heap", "rss")]
        rng.shuffle(jobs)
        for name, mode in jobs:
            worker = candidate if name == "candidate" else reference
            row[name + "_" + mode] = observe(worker, "memory", path, mode)
        samples.append(row)
    return evaluate(samples, pairs, seed, certificate, interval)


def evaluate(samples, pairs, seed, certificate, interval):
    rss_bounds = []
    heaps = []
    totals = []
    for row in samples:
        if any(v["status"] != "measured" for v in row.values()):
            return {"status": "measurement-failed", "samples": samples, "certificate": certificate}
        expected = row["reference_heap"]["work"]
        if any(v["work"] != expected for v in row.values()):
            return {"status": "unequal-memory-work", "samples": samples, "certificate": certificate}
        a, b = (row[k + "_rss"]["observer"] for k in ("candidate", "reference"))
        # Whole-process HWM bounds the operation even if a short-lived allocation
        # was missed by a sample. Charge two pages to candidate uncertainty and
        # remove two from the reference lower bound; never clamp a bad ratio.
        upper = max(a["hwm_end"], a["rss_peak_observed"]) - a["rss_start"] + 2 * a["page_size"]
        lower = b["rss_peak_observed"] - b["rss_start"] - 2 * b["page_size"]
        if lower <= 0 or upper <= 0:
            return {"status": "rss-attribution-unresolved", "samples": samples, "certificate": certificate}
        rss_bounds.append((upper, lower))
        heaps.append(tuple(row[k + "_heap"]["observer"]["heap_peak_bytes"] for k in ("candidate", "reference")))
        totals.append((max(a["hwm_end"], a["rss_peak_observed"]), max(b["hwm_end"], b["rss_peak_observed"])))
    try:
        rss, heap, total = (interval(values, seed) for values in (rss_bounds, heaps, totals))
    except ValueError:
        return {"status": "invalid-memory-observation", "samples": samples, "certificate": certificate}
    return {"status": "qualified", "rss_ratio_upper_95": rss["upper_95"], "heap_ratio_upper_95": heap["upper_95"],
            "rss_bound_ratio": rss, "heap_ratio": heap, "total_rss_ratio": total, "pairs": pairs,
            "samples": samples, "certificate": certificate, "seed": seed}
