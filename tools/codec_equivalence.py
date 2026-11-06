#!/usr/bin/env python3
"""Record exact decoder-output identities for the codec crates.

Runs each codec's `decode` example over the local conformance vectors and the
`codec-worker` raw operation over the frozen decoder workloads. It records the SHA-256 of
every output file plus the exit status and stderr. Refactors of decoders, bit
readers and sessions must reproduce the record exactly; this is equivalence
evidence only, never parity with or performance against FFmpeg.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.common import digest, digest_bytes, load, write_json
SUITE = ROOT / ".research/m1d4/fate-suite"
WORKLOADS = ROOT / ".research/d4-v3-numerical-activation-plan-v1/WORKLOADS.json"
DECODERS = {"aac": ["aac"], "ac3": ["ac3", "eac3"], "dca": ["dts"],
            "h264": ["h264-conformance"], "hevc": ["hevc-conformance"]}


def run(job):
    name, argv, output = job
    try:
        result = subprocess.run(argv, capture_output=True, timeout=600, check=False)
        status = result.returncode
        stderr = digest_bytes(result.stderr)
    except subprocess.TimeoutExpired:
        status, stderr = "timeout", None
    output_sha256 = digest(output) if os.path.exists(output) else None
    if os.path.exists(output):
        os.unlink(output)
    return name, {"exit": status, "output_sha256": output_sha256, "stderr_sha256": stderr}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--jobs", type=int, default=os.cpu_count())
    args = parser.parse_args()
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "codec-worker"],
                   cwd=ROOT, env=env, check=True)
    examples = ROOT / "target/release/examples"
    jobs = []
    with tempfile.TemporaryDirectory() as scratch:
        for codec, folders in DECODERS.items():
            binary = examples / "decode"
            # One example name per package: cargo overwrites target/release/examples/decode,
            # so build each package's example into its own copy first.
            subprocess.run(["cargo", "build", "--release", "--locked", "--example",
                            "decode", "-p", f"triergon-codec-{codec}"], cwd=ROOT, env=env, check=True,
                           capture_output=True)
            copy = Path(scratch) / f"decode-{codec}"
            copy.write_bytes(binary.read_bytes())
            copy.chmod(0o755)
            for folder in folders:
                for vector in sorted((SUITE / folder).rglob("*")):
                    if vector.is_file():
                        out = Path(scratch) / f"{codec}-{len(jobs)}.out"
                        name = f"{codec}/{vector.relative_to(SUITE)}"
                        jobs.append((name, [str(copy), str(vector), str(out), *(["--quiet"] if codec == "hevc" else [])], str(out)))
        for workload in load(WORKLOADS)["workloads"]:
            out = Path(scratch) / f"worker-{workload['id']}.out"
            jobs.append(("worker/" + workload["id"], [str(ROOT / "target/release/codec-worker"),
                         workload["operation"] + "-raw", str(ROOT / workload["path"]), str(out)], str(out)))
        with ThreadPoolExecutor(args.jobs) as pool:
            record = dict(sorted(pool.map(run, jobs)))
    write_json(args.output, {"schema": 1, "outputs": record}, indent=1)
    print(len(record), "decoder outputs;", sum(r["exit"] == 0 for r in record.values()), "exit 0")
    if args.compare:
        before = load(args.compare)["outputs"]
        changed = sorted(k for k in before.keys() | record.keys() if before.get(k) != record.get(k))
        if changed:
            print("NOT EQUIVALENT", len(changed), changed[:20])
            return 1
        print("byte-identical to", args.compare)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
