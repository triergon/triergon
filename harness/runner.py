"""Shared adapters, canonical comparisons and fail-closed acceptance.

Only Python's standard library is used. Native reference workers stay external.
"""
from __future__ import annotations

import contextlib
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import stat
import statistics
import subprocess
import tempfile
import time
from .spec_oracle import timing_trace

ROOT = Path(__file__).resolve().parents[1]
PROC_ROOT = Path("/proc")
LINUX_CPU_KEY = "__linux_cpu__"
LINUX_MODULE_ROOT = Path("/sys/module")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def load(path: Path):
    return json.loads(path.read_text())


def measurement_cache(report):
    """Include unvisited measurements retained through an interrupted replay."""
    rows = [*report.get("cached_cases", []), *report.get("cases", [])]
    cache = {row["id"]: row for row in rows}
    if len(cache) != len(rows):
        raise ValueError("duplicate measurement-cache case identities")
    return cache


def checkpoint_report(base, rows, reusable):
    # A short isolation window can expire before a later cached case is reached.
    # Keep that original observation (including a clean failure), but distinguish
    # it from source comparisons actually rerun in this collection attempt.
    visited = {row["id"] for row in rows}
    return {**base, "collection_status": "partial", "cases": list(rows),
            "cached_cases": [row for name, row in reusable.items() if name not in visited],
            "acceptance": {"accepted": False, "slice_accepted": False,
                           "slice_reasons": ["collection incomplete"], "reasons": ["collection incomplete"]}}


def execute(argv, timeout=30):
    """Retain failures; never synthesize a successful result for a failed worker."""
    started = time.monotonic_ns()
    try:
        result = subprocess.run(list(map(str, argv)), capture_output=True, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        return {"status": "timeout", "argv": list(map(str, argv))}
    if result.returncode:
        return {"status": "worker-error", "exit_code": result.returncode, "stderr": result.stderr.decode(errors="replace")[-8192:]}
    try:
        data = json.loads(result.stdout)
    except (ValueError, UnicodeError):
        return {"status": "invalid-worker-output"}
    if not isinstance(data, dict):
        return {"status": "invalid-worker-output"}
    return {"status": "ok", "data": data, "process_elapsed_ns": time.monotonic_ns() - started}


def differences(actual, expected, path="", maximum=40, *, display_value=None):
    """Strict typed comparison; no timestamp rounding or dropping unknown fields."""
    found = []

    def shown(value):
        if display_value is not None:
            return display_value(value)
        return value if len(str(value)) < 256 else "<large value>"

    def walk(a, b, where):
        if len(found) >= maximum:
            return
        # JSON integer vs exactly equal integral float is intentional for sample rate.
        if isinstance(a, bool) != isinstance(b, bool):
            found.append({"path": where, "actual": a if display_value is None else shown(a),
                          "expected": b if display_value is None else shown(b)})
        elif isinstance(a, dict) and isinstance(b, dict):
            if a.keys() != b.keys():
                found.append({"path": where, "actual_keys": sorted(a), "expected_keys": sorted(b)})
            for key in sorted(a.keys() & b.keys()):
                walk(a[key], b[key], where + "/" + key)
        elif isinstance(a, list) and isinstance(b, list):
            if len(a) != len(b):
                found.append({"path": where, "actual_length": len(a), "expected_length": len(b)})
            for i, (left, right) in enumerate(zip(a, b)):
                walk(left, right, where + "/" + str(i))
        elif a != b:
            found.append({"path": where, "actual": shown(a), "expected": shown(b)})

    walk(actual, expected, path)
    return found


def review_native_difference(storage, native, comparison):
    delta = differences(storage, native)
    actual_sha = hashlib.sha256(json.dumps(native, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    expected = comparison.get("raw_native_output_sha256")
    reviewed = bool(expected) and actual_sha == expected and [d["path"] for d in delta] == comparison["raw_native_difference_paths"]
    disposition = {"status": "reviewed" if reviewed else "unexpected-native-difference", "rule": comparison["id"]}
    return delta, disposition, actual_sha


def parity(candidate: Path, reference: Path, path: Path, raw: Path, sequential_required=True,
           comparison=None, native_reference=None, source_content=False, trace_prefix=""):
    
    storage = execute([candidate, trace_prefix+"trace", path])
    left = execute([candidate, "codec-trace", path]) if comparison else storage
    right = execute([reference, "codec-trace" if comparison else trace_prefix+"trace", path])
    raw.mkdir(parents=True, exist_ok=True)
    for name, result in (("candidate", left), ("reference", right), ("source-frame-candidate", storage)):
        (raw / (name + ".json")).write_text(json.dumps(result, indent=2) + "\n")
    if any(result["status"] != "ok" for result in (left, right, storage)):
        return {"status": "worker-failure", "candidate": left["status"], "reference": right["status"]}
    diff = differences(left["data"], right["data"])
    raw_native_diff, disposition, raw_native_sha = [], None, None
    if comparison:
        native = execute([native_reference, "trace", path])
        (raw / "raw-native-reference.json").write_text(json.dumps(native, indent=2) + "\n")
        if native["status"] != "ok":
            return {"status": "worker-failure", "reference": "raw native operation failed"}
        raw_native_diff, disposition, raw_native_sha = review_native_difference(storage["data"], native["data"], comparison)
    content_status, content_diff = "not-required", []
    if source_content or comparison:
        from .content_oracle import content_trace, projected_content
        import zlib
        try:
            content_expected = content_trace(path)
            content_actual = projected_content(storage["data"])
            content_diff = differences(content_actual, content_expected)
            content_status = "pass" if not content_diff else "mismatch"
        except (ValueError, IndexError, KeyError, StopIteration, zlib.error) as error:
            content_expected = None
            content_status, content_diff = "oracle-error", [{"error": str(error)}]
        (raw / "source-content-oracle.json").write_text(json.dumps(content_expected, indent=2) + "\n")
    source = execute([candidate, "trace-source", path])
    try:
        oracle = timing_trace(path)
        source_diff = differences(source.get("data"), oracle)
        source_status = "pass" if source["status"] == "ok" and not source_diff else "mismatch"
    except (ValueError, IndexError, KeyError, StopIteration) as error:
        source_status, source_diff = "oracle-error", [{"error": str(error)}]
        oracle = None
    (raw / "source-candidate.json").write_text(json.dumps(source, indent=2) + "\n")
    (raw / "source-oracle.json").write_text(json.dumps(oracle, indent=2) + "\n")
    streamed = []
    for chunk in ((1, 7, 4096) if sequential_required else ()):
        short = execute([candidate, trace_prefix+"trace-short", path, chunk])
        mismatch = short["status"] != "ok" or bool(differences(short.get("data"), storage["data"]))
        if comparison:
            projected = execute([candidate, "codec-trace-short", path, chunk])
            mismatch |= projected["status"] != "ok" or bool(differences(projected.get("data"), left["data"]))
        exact_short = execute([candidate, "trace-source-short", path, chunk])
        mismatch |= exact_short["status"] != "ok" or bool(differences(exact_short.get("data"), oracle))
        streamed.append({"chunk": chunk, "status": "mismatch" if mismatch else "pass"})
    seeks = []
    packets = right["data"]["packets"]
    video = next((i for i, t in enumerate(right["data"]["tracks"]) if t["kind"] == 1), None)
    if video is not None:
        for target in (0, 250_000_000, 1_500_000_000, 20_000_000_000):
            valid = [p for p in packets if p["track"] == video and p["key"] and p["pts_ns"] is not None and p["pts_ns"] <= target]
            if not valid:
                seeks.append({"target_ns": target, "status": "no-previous-keyframe"})
                continue
            expected = max(valid, key=lambda p: p["pts_ns"])
            actual = execute([candidate, "seek", path, target])
            wanted = {"actual_ns": expected["pts_ns"], **{k: expected[k] for k in ("track", "pts_ns", "key", "payload")}}
            delta = differences(actual.get("data"), wanted)
            seeks.append({"target_ns": target, "status": "pass" if actual["status"] == "ok" and not delta else "mismatch", "differences": delta})
    passed = source_status == "pass" and content_status in ("pass", "not-required") and not diff and all(s["status"] == "pass" for s in streamed) and all(s["status"] in ("pass", "no-previous-keyframe") for s in seeks)
    passed &= not comparison or disposition["status"] == "reviewed"
    return {"status": "pass" if passed else "mismatch", "differences": diff,
            "source_timing_status": source_status, "source_timing_differences": source_diff,
            "source_content_status": content_status, "source_content_differences": content_diff,
            "comparison_rule": comparison["id"] if comparison else None,
            "raw_native_differences": raw_native_diff, "raw_native_disposition": disposition,
            "raw_native_output_sha256": raw_native_sha,
            "short_reads": streamed, "seeks_after_eof": seeks,
            "packets": len(packets), "bytes": sum(len(p["payload"]) // 2 for p in packets), "sequential_required": sequential_required}


def catalogue_parity(candidate,reference,path,output,**kwargs):
    result=parity(candidate,reference,path,output,trace_prefix="catalogue-",**kwargs)
    from .catalogue_oracle import source_catalogue
    from .colour_oracle import source_colour
    actual=execute([candidate,"catalogue-source",path])
    colour=execute([candidate,"metadata-source",path])
    try:
        expected=source_catalogue(path);delta=differences(actual.get("data"),expected)
        state="pass" if actual["status"]=="ok" and not delta else "mismatch"
        colour_expected=source_colour(path);colour_delta=differences(colour.get("data"),colour_expected)
        colour_state="pass" if colour["status"]=="ok" and not colour_delta else "mismatch"
    except (ValueError,IndexError,KeyError,StopIteration) as error:
        expected=None;colour_expected=None;delta=[{"error":str(error)}];colour_delta=delta;state=colour_state="oracle-error"
    for name,value in [("catalogue-source",actual),("catalogue-oracle",expected),("colour-source",colour),("colour-oracle",colour_expected)]:
        (output/(name+".json")).write_text(json.dumps(value,indent=2)+"\n")
    result.update({"source_catalogue_status":state,"source_catalogue_differences":delta,"source_colour_status":colour_state,"source_colour_differences":colour_delta})
    if state!="pass" or colour_state!="pass":result["status"]="mismatch"
    return result


def colour_parity(candidate, reference, path, output, **kwargs):
    # Keep the complete existing packet/time/short-read/seek proof as well.
    result=parity(candidate,reference,path,output,**kwargs)
    left=execute([candidate,"metadata-trace",path]);right=execute([reference,"metadata-trace",path])
    source=execute([candidate,"metadata-source",path])
    from .colour_oracle import source_colour
    try:
        oracle=source_colour(path)
        delta=differences(source.get("data"),oracle)
        source_status="pass" if source["status"]=="ok" and not delta else "mismatch"
    except (ValueError,IndexError,KeyError,StopIteration) as error:
        oracle=None;delta=[{"error":str(error)}];source_status="oracle-error"
    native_delta=differences(left.get("data"),right.get("data"))
    native_status="pass" if left["status"]==right["status"]=="ok" and not native_delta else "mismatch"
    shorts=[]
    for chunk in (1,7,4096):
        actual=execute([candidate,"metadata-trace-short",path,chunk])
        shorts.append({"chunk":chunk,"status":"pass" if actual["status"]=="ok" and not differences(actual.get("data"),left.get("data")) else "mismatch"})
    for name,value in [("colour-candidate",left),("colour-native",right),("colour-source",source),("colour-oracle",oracle)]:
        (output/(name+".json")).write_text(json.dumps(value,indent=2)+"\n")
    result.update({"source_colour_status":source_status,"source_colour_differences":delta,"colour_reference_status":native_status,"colour_reference_differences":native_delta,"colour_short_reads":shorts})
    if source_status!="pass" or native_status!="pass" or any(s["status"]!="pass" for s in shorts):result["status"]="mismatch"
    return result


def seek_parity(candidate, reference, path, output, **kwargs):
    result=parity(candidate,reference,path,output, **kwargs)
    comparisons=[]
    for target in [0,250_000_000,2_500_000_000,7_500_000_000,15_500_000_000,25_000_000_000]:
        for state in [[],["after-eof"]]:
            left=execute([candidate,"seek-trace",path,target,*state])
            right=execute([reference,"seek-trace",path,target,*state])
            delta=differences(left.get("data"),right.get("data"))
            status="pass" if left["status"]==right["status"]=="ok" and not delta else "mismatch"
            row={"target_ns":target,"state":"after-eof" if state else "fresh","status":status,"differences":delta,"candidate":left,"reference":right}
            comparisons.append(row)
    (output/"seek-comparisons.json").write_text(json.dumps(comparisons,indent=2)+"\n")
    result["playback_seeks"]=[{k:v for k,v in row.items() if k not in ("candidate","reference")} for row in comparisons]
    if any(row["status"]!="pass" for row in comparisons):result["status"]="mismatch"
    return result


def percentile(values, fraction):
    ordered = sorted(values)
    index = (len(ordered) - 1) * fraction
    lo = math.floor(index)
    hi = math.ceil(index)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (index - lo)


def ratio_interval(pairs, seed=0, resamples=4000):
    if len(pairs) < 2 or any(not math.isfinite(a) or not math.isfinite(b) or a <= 0 or b <= 0 for a, b in pairs):
        raise ValueError("ratios require positive finite paired measurements")
    ratios = [a / b for a, b in pairs]
    rng = random.Random(seed)
    estimates = [statistics.median(rng.choices(ratios, k=len(ratios))) for _ in range(resamples)]
    return {"median": statistics.median(ratios), "lower_95": percentile(estimates, .025), "upper_95": percentile(estimates, .975)}


@contextlib.contextmanager
def benchmark_lock():
    # Same host-wide name for all worktrees, not one lock per checkout.
    path = Path(tempfile.gettempdir()) / "safe-media-bench.lock"
    with path.open("a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        lock.seek(0); lock.truncate(); lock.write(str(os.getpid())); lock.flush()
        yield


def linux_cpu_snapshot():
    evidence = {"status": "unavailable", "platform": "Linux"}
    result = {LINUX_CPU_KEY: evidence}
    try:
        commands = subprocess.check_output(["ps", "-A", "-o", "pid=,time=,state=,comm="], text=True)
        evidence["ps_raw"] = commands
        raw = (PROC_ROOT / "stat").read_text()
        evidence["proc_stat_raw"] = raw
        aggregate = [line.split() for line in raw.splitlines() if line.startswith("cpu ")]
        if len(aggregate) != 1 or len(aggregate[0]) < 9:
            raise ValueError("missing aggregate /proc/stat steal counter")
        counters = [int(value) for value in aggregate[0][1:]]
        if any(value < 0 for value in counters):
            raise ValueError("negative /proc/stat counter")
        cpus = [line.split()[0] for line in raw.splitlines() if re.match(r"cpu\d+\s", line)]
        if not cpus or len(cpus) != len(set(cpus)):
            raise ValueError("missing or duplicate per-CPU accounting")
        boot = (PROC_ROOT / "sys/kernel/random/boot_id").read_text().strip()
        ticks = os.sysconf("SC_CLK_TCK")
        if not boot or type(ticks) is not int or ticks <= 0:
            raise ValueError("missing boot/tick identity")
        evidence.update(counters=counters, cpus=cpus, boot_id=boot, ticks_per_second=ticks)
        for line in commands.splitlines():
            if not line.strip():
                continue
            fields = line.split(maxsplit=3)
            if len(fields) != 4:
                raise ValueError("unparsed Linux ps record: " + line)
            pid, clock, state, command = fields
            name = Path(command).name
            if not re.fullmatch(r"(?:rustc|cargo|clang(?:\+\+)?(?:-\d+)?|cc|gcc(?:-\d+)?|g\+\+(?:-\d+)?|cc1(?:plus)?|ld(?:\.(?:lld|bfd|gold))?|rust-lld|lto(?:1)?|ffmpeg|as|make|gmake|ninja|cmake)", name):
                continue
            process_raw = (PROC_ROOT / pid / "stat").read_text()
            head, separator, tail = process_raw.rpartition(")")
            values = tail.split()
            if not separator or not head.startswith(pid + " (") or len(values) < 20:
                raise ValueError("unparsed Linux process stat: " + pid)
            utime, stime, start = (int(values[index]) for index in (11, 12, 19))
            if min(utime, stime, start) < 0:
                raise ValueError("negative Linux process clock")
            result[int(pid)] = {"name": name, "cpu_ticks": utime + stime, "state": values[0],
                                "start_ticks": start, "ps_clock_raw": clock, "ps_state_raw": state,
                                "proc_stat_raw": process_raw}
        evidence["status"] = "recorded"
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        evidence["error"] = str(error)
    return result


def cpu_snapshot():
    if platform.system() == "Linux":
        return linux_cpu_snapshot()
    commands = subprocess.check_output(["ps", "-A", "-o", "pid=,time=,state=,comm="], text=True)
    result = {}
    for line in commands.splitlines():
        fields = line.split(maxsplit=3)
        if len(fields) != 4:
            continue
        pid, clock, state, command = fields
        name = Path(command).name
        if name not in {"rustc", "cargo", "clang", "cc", "gcc", "ld", "lto", "ffmpeg"}:
            continue
        seconds = 0
        for component in clock.split(":"):
            seconds = seconds * 60 + float(component)
        result[int(pid)] = {"name": name, "cpu_ticks": round(seconds * 100), "state": state}
    return result


def linux_cpu_findings(before, after, required):
    first, last = before.get(LINUX_CPU_KEY), after.get(LINUX_CPU_KEY)
    if first is None or last is None:
        return ["linux-cpu-evidence-missing"] if required or first is not None or last is not None else []
    if not isinstance(first, dict) or not isinstance(last, dict) or any(row.get("status") != "recorded" for row in (first, last)):
        return ["linux-cpu-evidence-unavailable"]
    try:
        for row in (first, last):
            if (not row.get("boot_id") or not row.get("cpus") or type(row.get("ticks_per_second")) is not int
                or row["ticks_per_second"] <= 0 or len(row["counters"]) < 8
                or any(type(value) is not int or value < 0 for value in row["counters"])):
                raise ValueError("invalid Linux CPU identity/counters")
        if (any(first[key] != last[key] for key in ("boot_id", "cpus", "ticks_per_second"))
            or len(first["counters"]) != len(last["counters"])
            or any(a > b for a, b in zip(first["counters"], last["counters"]))):
            raise ValueError("Linux CPU accounting changed or regressed")
    except (KeyError, TypeError, ValueError):
        return ["linux-cpu-evidence-invalid"]
    return ["linux-cpu-steal"] if last["counters"][7] != first["counters"][7] else []


def active_cpu(before, after, *, require_linux=False):
    findings = linux_cpu_findings(before, after, require_linux)
    before = {key: value for key, value in before.items() if key != LINUX_CPU_KEY}
    after = {key: value for key, value in after.items() if key != LINUX_CPU_KEY}
    names = {before[pid]["name"] for pid in before.keys() - after.keys()}
    names.update(item["name"] for pid, item in after.items()
                 if pid not in before or item["cpu_ticks"] != before[pid]["cpu_ticks"]
                 or item.get("start_ticks") != before[pid].get("start_ticks")
                 or "R" in item["state"] or "R" in before.get(pid,{}).get("state",""))
    names.update(findings)
    return sorted(names)


def competing_builds():
    before = cpu_snapshot()
    time.sleep(.125)
    return active_cpu(before, cpu_snapshot())


def benchmark(candidate, reference, path, pairs, iterations, seed, operation=None):
    rejected = []
    for attempt in range(3):
        result = benchmark_once(candidate, reference, path, pairs, iterations, seed + attempt, operation)
        if result.get("status") != "contaminated":
            result["rejected_contaminated_attempts"] = rejected
            return result
        rejected.append(result)
    return {"status":"contaminated", "rejected_contaminated_attempts":rejected, "contaminants":result.get("contaminants",[])}


def timing_module_state():
    """The external RSS kprobes may not be loaded during timing, even unarmed.

    This is a launch/boundary guard, not permission to reclassify old captures.
    The parent's global module/load profile also serializes the lifecycle.
    """
    if platform.system() != "Linux":
        return {"status": "not-applicable", "platform": platform.system()}
    module = LINUX_MODULE_ROOT / "safe_media_rss_bound"
    try:
        root = LINUX_MODULE_ROOT.stat()
        if not stat.S_ISDIR(root.st_mode):
            raise ValueError("Linux module sysfs root is not a directory")
        try:
            loaded = module.stat()
        except FileNotFoundError:
            after = LINUX_MODULE_ROOT.stat()
            if not stat.S_ISDIR(after.st_mode) or (root.st_dev, root.st_ino) != (after.st_dev, after.st_ino):
                raise ValueError("Linux module sysfs root changed during observation")
            return {"status": "module-absent", "path": str(module),
                    "sysfs_root_device": root.st_dev, "sysfs_root_inode": root.st_ino}
        return {"status": "module-loaded", "path": str(module), "mode": loaded.st_mode,
                "device": loaded.st_dev, "inode": loaded.st_ino}
    except (OSError, ValueError) as error:
        return {"status": "unavailable", "path": str(module), "error": str(error)}


def benchmark_once(candidate, reference, path, pairs, iterations, seed, operation=None):
    if pairs < 2 or iterations < 1:
        raise ValueError("need at least two pairs and one iteration")
    prefix, arguments = operation or ("", [])
    workers = {"candidate": candidate, "reference": reference}
    rng = random.Random(seed)
    samples = []
    contaminants = []
    module_observations, warmups = [], []
    def module_rejection(stage):
        state = timing_module_state()
        module_observations.append({"stage": stage, "state": state})
        if state.get("status") in {"module-absent", "not-applicable"}:
            return None
        return {"status": "timing-module-loaded" if state.get("status") == "module-loaded" else "timing-environment-invalid",
                "samples": samples, "warmups": warmups, "timing_module_observations": module_observations,
                "timing_module_policy": "rss-kernel-module-must-be-absent-v1"}
    with benchmark_lock():
        if rejection := module_rejection("before-timing-session"):
            return rejection
        if busy := competing_builds():
            return {"status": "contaminated", "contaminants": busy, "samples": []}
        worker_hashes = {name: digest(Path(worker)) for name, worker in workers.items()}
        input_sha256 = digest(Path(path))
        for name, worker in workers.items():
            if rejection := module_rejection("before-warmup:" + name):
                return rejection
            warm = execute([worker, prefix+"bench", path, iterations, *arguments])
            warmups.append({"worker": name, "result": warm})
            if rejection := module_rejection("after-warmup:" + name):
                return rejection
            if warm["status"] != "ok":
                return {"status": "warmup-failed", "result": warm}
        for index in range(pairs):
            if busy := competing_builds():
                return {"status": "contaminated", "contaminants": busy, "samples": samples}
            order = list(workers); rng.shuffle(order)
            before = cpu_snapshot()
            row = {"pair": index, "order": order, "load": os.getloadavg(), "cpu_before": before}
            samples.append(row)
            for name in order:
                if rejection := module_rejection("before-pair:" + str(index) + ":" + name):
                    return rejection
                row[name] = execute([workers[name], prefix+"bench", path, iterations, *arguments])
                if rejection := module_rejection("after-pair:" + str(index) + ":" + name):
                    return rejection
            after = cpu_snapshot()
            row["cpu_after"] = after
            if busy := active_cpu(before, after):
                return {"status": "contaminated", "contaminants": busy, "samples": samples}
    values = []
    for row in samples:
        if any(row[k]["status"] != "ok" for k in workers):
            return {"status": "worker-failure", "samples": samples}
        a, b = (row[k]["data"] for k in workers)
        if any(a[k] != b[k] for k in ("iterations", "packets", "bytes", "digest")):
            return {"status": "unequal-work", "samples": samples}
        values.append((a["elapsed_ns"], b["elapsed_ns"]))
    if worker_hashes != {name: digest(Path(worker)) for name, worker in workers.items()} or input_sha256 != digest(Path(path)):
        return {"status": "worker-or-input-changed", "samples": samples}
    summary = ratio_interval(values, seed)
    return {"status": "measured", "worker_hashes": worker_hashes, "input_sha256": input_sha256,
            "seed": seed, "iterations": iterations, "pairs": pairs,
            "ratio": summary, "samples": samples, "contaminants": sorted(set(contaminants)),
            "timing_module_observations": module_observations,
            "timing_module_policy": "rss-kernel-module-must-be-absent-v1",
            "per_iteration_ns": {name: {"p50": statistics.median(row[name]["data"]["elapsed_ns"] / iterations for row in samples), "p95": percentile([row[name]["data"]["elapsed_ns"] / iterations for row in samples], .95)} for name in workers}}


def memory(candidate, reference, path, pairs, seed, operation=None):
    from .memory import qualification, measure
    with benchmark_lock():
        if busy := competing_builds():
            return {"status": "contaminated", "contaminants": busy, "samples": []}
        before = cpu_snapshot()
        certificate = qualification(candidate, reference)
        result = measure(candidate, reference, path, pairs, seed, certificate, ratio_interval, operation)
        after = cpu_snapshot()
        contaminants = active_cpu(before, after)
        result = {**result, "cpu_before": before, "cpu_after": after, "contaminants": contaminants}
        if contaminants:
            result["measurement_status"] = result["status"]
            result["status"] = "contaminated"
        return result


def acceptance(contract, rows, identity, target, coverage, additional_cases=()):
    reasons = []
    if len({r["id"] for r in rows}) != len(rows):
        reasons.append("duplicate case identities")
    if not rows or any(row.get("parity", {}).get("status") != "pass" or row.get("parity", {}).get("source_timing_status") != "pass" for row in rows):
        reasons.append("selected-feature parity incomplete or failing")
    profiles = {row.get("profile") for row in rows if row.get("parity", {}).get("status") == "pass"}
    missing = sorted(set(contract["required_profiles"]) - profiles)
    if missing:
        reasons.append("missing required profiles: " + ", ".join(missing))
    for row in rows:
        from .comparison import validate_case_evidence
        try:
            selected_hashes = validate_case_evidence(row, identity, contract.get("id"))
        except ValueError as error:
            reasons.append(row["id"] + ": " + str(error))
            selected_hashes = {}
        if contract.get("id") == "K2-bounded-zlib-v1" and row.get("parity", {}).get("source_content_status") != "pass":
            reasons.append(row["id"] + ": complete source content gate unproven or failed")
        if contract.get("id") == "K4-colour-hdr-v1":
            proof=row.get("parity",{})
            if proof.get("source_colour_status")!="pass" or proof.get("colour_reference_status")!="pass" or proof.get("colour_short_reads") != [{"chunk":n,"status":"pass"} for n in (1,7,4096)]:
                reasons.append(row["id"] + ": complete source/native colour gate unproven or failed")
            if row.get("operation",{}).get("view")!="colour-source-and-ffmpeg9-v1":
                reasons.append(row["id"] + ": colour benchmark operation view unproven")
        if contract.get("id")=="K5-segment-metadata-v1":
            proof=row.get("parity",{})
            if proof.get("source_catalogue_status")!="pass" or proof.get("source_colour_status")!="pass" or proof.get("short_reads")!=[{"chunk":n,"status":"pass"} for n in (1,7,4096)]:
                reasons.append(row["id"]+": complete source/native catalogue gate unproven or failed")
            if row.get("operation",{}).get("view")!="catalogue-source-and-native-v1":reasons.append(row["id"]+": catalogue timing view unproven")
        timing = row.get("timing", {})
        upper = timing.get("ratio", {}).get("upper_95", math.inf)
        raw = timing.get("samples", [])
        iterations = timing.get("iterations")
        valid_raw = type(timing.get("pairs")) is int and len(raw) == timing["pairs"] and len(raw) >= contract["minimum_pairs"]
        valid_raw &= type(iterations) is int and iterations > 0
        valid_raw &= timing.get("worker_hashes") == selected_hashes
        if row.get("input_sha256"):
            valid_raw &= timing.get("input_sha256") == row["input_sha256"]
        if valid_raw:
            try:
                paired = []
                for index, sample in enumerate(raw):
                    if type(sample.get("pair")) is not int or sample["pair"] != index:
                        raise ValueError("missing, duplicate or reordered timing pair identity")
                    if "cpu_before" not in sample or "cpu_after" not in sample or active_cpu(sample["cpu_before"],sample["cpu_after"], require_linux=identity.get("host_platform", {}).get("system") == "Linux" or "linux" in target):
                        raise ValueError("CPU isolation evidence missing or contaminated")
                    a, b = (sample[k]["data"] for k in ("candidate", "reference"))
                    if any(type(work[k]) is not int or work[k] < 0
                           for work in (a, b) for k in ("iterations", "packets", "bytes", "digest", "elapsed_ns")):
                        raise ValueError("untyped benchmark work")
                    if a["iterations"] != iterations or b["iterations"] != iterations:
                        raise ValueError("raw iterations differ from declared operation count")
                    if any(sample[k]["status"] != "ok" for k in ("candidate", "reference")) or any(a[k] != b[k] for k in ("iterations", "packets", "bytes", "digest")):
                        raise ValueError("unequal benchmark work")
                    paired.append((a["elapsed_ns"], b["elapsed_ns"]))
                if ratio_interval(paired, timing.get("seed", 0)) != timing.get("ratio"):
                    valid_raw = False
            except (KeyError, ValueError, TypeError):
                valid_raw = False
        if timing.get("status") != "measured" or not valid_raw or timing.get("contaminants") or not math.isfinite(upper) or upper <= 0 or upper > contract["runtime_ratio_max"]:
            reasons.append(row["id"] + ": runtime gate unproven or failed")
        mem = row.get("memory", {})
        memory_ratios = [mem.get(k, math.inf) for k in ("rss_ratio_upper_95", "heap_ratio_upper_95")]
        valid_memory = False
        try:
            from .memory import evaluate
            certificate = mem["certificate"]
            raw_memory = mem["samples"]
            probe = certificate["probe_heap"]
            covered = certificate["status"] == "qualified-selected-workers" and not certificate["errors"] and certificate["worker_hashes"] == selected_hashes
            if identity.get("schema", 1) >= 2:
                covered &= not mem.get("contaminants") and not active_cpu(mem["cpu_before"], mem["cpu_after"], require_linux=identity.get("host_platform", {}).get("system") == "Linux" or "linux" in target)
            if row.get("input_sha256"):
                covered &= mem.get("input_sha256") == row["input_sha256"]
            instrumentation = identity.get("instrumentation")
            if instrumentation:
                covered &= certificate.get("probe_sha256") == instrumentation.get("probe_sha256")
                covered &= certificate.get("observer_sha256") == instrumentation.get("observer_sha256")
            covered &= probe["status"] == "measured" and probe["observer"]["heap_peak_bytes"] == probe["work"]["expected_peak_bytes"]
            covered &= probe["observer"]["heap_live_start"] == probe["observer"]["heap_live_end"]
            if covered and len(raw_memory) == mem["pairs"] and len(raw_memory) >= contract["minimum_pairs"]:
                covered &= all(v["instrument_sha256"] == certificate["observer_sha256"] and v["observer"]["status"] == "measured" for sample in raw_memory for v in sample.values())
                recomputed = evaluate(raw_memory,mem["pairs"],mem.get("seed",280),certificate,ratio_interval)
                valid_memory = covered and all(recomputed.get(k) == mem.get(k) for k in ("status","rss_ratio_upper_95","heap_ratio_upper_95","rss_bound_ratio","heap_ratio","total_rss_ratio"))
        except (KeyError, ValueError, TypeError, ZeroDivisionError):
            pass
        if not valid_memory or mem.get("status") != "qualified" or any(not math.isfinite(r) or not 0 < r < 1 for r in memory_ratios):
            reasons.append(row["id"] + ": lower RSS/live allocation gate unproven or failed")
    # Required intact external vectors use the same selected-feature/numerical gates.
    # Their redistribution status remains a separate, still-open module requirement.
    for spec in contract.get("required_additional_vectors",[]):
        matches=[case for case in additional_cases if case.get("id")==spec["file"] and case.get("profile")==spec["file"]]
        if len(matches)!=1 or matches[0].get("input_sha256")!=spec["sha256"]:
            reasons.append("required additional vector missing or changed: "+spec["file"])
            continue
        derived={**contract,"required_profiles":[spec["file"]],"required_additional_vectors":[]}
        extra=acceptance(derived,matches,identity,target,coverage)
        reasons.extend("additional "+spec["file"]+": "+reason for reason in extra["slice_reasons"])
    if identity.get("source_revision") is None:
        reasons.append("FFmpeg source build/configuration is not qualified")
    if set(contract["required_targets"]) != {target}:
        reasons.append("required cross-target evidence missing")
    slice_reasons = list(reasons)
    if not coverage or any(c["status"] != "pass" for c in coverage):
        reasons.append("FATE mapping/asset/expected-output coverage pending")
    if contract.get("pending_module_requirements"):
        reasons.append("broader module requirements remain pending")
    return {"accepted": not reasons, "slice_accepted": not slice_reasons, "slice_reasons": slice_reasons, "reasons": reasons, "missing_profiles": missing}


def fate_coverage(index, mapping, cache=None):
    rules = {r["id"]: r for r in mapping["rules"]}
    rows = []
    build = ROOT / ".research/reference/build.json"
    enabled_path = ROOT / ".research/reference/fate-list.txt"
    enabled = set(enabled_path.read_text().split()) if enabled_path.exists() else set()
    receipts = {r["case"]:r for path in (ROOT/".research/reference").glob("fate-*.json") if "case" in (r:=load(path))}
    for case in index["cases"]:
        rule = rules.get(case["id"])
        row = {"id": case["id"], "status": "unmapped", "definitions": case["definition_files"], "enabled_in_reference_configuration": case["id"] in enabled}
        if rule:
            row.update({"status": rule["status"], "operation": rule["operation"], "sample": rule["sample"]})
            if cache:
                path = (cache / rule["sample"]).resolve()
                if not path.is_relative_to(cache.resolve()):
                    raise ValueError("sample escapes cache")
                row["asset"] = {"status": "present-external-rights-unreviewed", "sha256": digest(path)} if path.is_file() else {"status": "missing"}
            receipt=receipts.get(case["id"],{})
            if receipt and build.exists() and receipt.get("revision") == load(build)["revision"]:
                row["upstream_reference_execution"] = receipt
        rows.append(row)
    return rows
