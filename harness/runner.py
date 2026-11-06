"""Worker execution, canonical comparisons and timing isolation primitives.

The codec and capture timing runners use these; the collectors themselves are
`cargo xtask report`. Only the standard library is used.
"""
from __future__ import annotations

import contextlib
import fcntl
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
from .common import canonical_digest, write_json
from .spec_oracle import timing_trace

ROOT = Path(__file__).resolve().parents[1]
PROC_ROOT = Path("/proc")
LINUX_CPU_KEY = "__linux_cpu__"
LINUX_MODULE_ROOT = Path("/sys/module")


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
    actual_sha = canonical_digest(native)
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
        write_json(raw / (name + ".json"), result)
    if any(result["status"] != "ok" for result in (left, right, storage)):
        return {"status": "worker-failure", "candidate": left["status"], "reference": right["status"]}
    diff = differences(left["data"], right["data"])
    raw_native_diff, disposition, raw_native_sha = [], None, None
    if comparison:
        native = execute([native_reference, "trace", path])
        write_json(raw / "raw-native-reference.json", native)
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
        write_json(raw / "source-content-oracle.json", content_expected)
    source = execute([candidate, "trace-source", path])
    try:
        oracle = timing_trace(path)
        source_diff = differences(source.get("data"), oracle)
        source_status = "pass" if source["status"] == "ok" and not source_diff else "mismatch"
    except (ValueError, IndexError, KeyError, StopIteration) as error:
        source_status, source_diff = "oracle-error", [{"error": str(error)}]
        oracle = None
    write_json(raw / "source-candidate.json", source)
    write_json(raw / "source-oracle.json", oracle)
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
        write_json(output/(name+".json"), value)
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
        write_json(output/(name+".json"), value)
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
    write_json(output/"seek-comparisons.json", comparisons)
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
