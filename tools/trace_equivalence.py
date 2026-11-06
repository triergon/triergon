#!/usr/bin/env python3
"""Record byte-level candidate trace identities over every recorded Matroska case.

The refactor programme (docs/refactor-plan.md) requires byte-identical worker
traces. This runs the harness's own parity operations for all six frozen slice
manifests, the mandatory HDR vector and the selected FATE vectors, and records
the SHA-256 of every raw candidate stdout together with the parity verdict.
Two records are equivalent only when every candidate invocation matches.
It measures nothing and accepts nothing; timing and memory stay separate.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness import runner
from harness.common import digest, digest_bytes, load, write_json
from harness.comparison import reference_record, rule_for

SLICES = [
    ("harness/corpus.json", "harness/slice.json", ".research/fixtures"),
    ("harness/header-corpus.json", "harness/header-slice.json", ".research/header-fixtures"),
    ("harness/zlib-corpus.json", "harness/zlib-slice.json", ".research/zlib-fixtures"),
    ("harness/seek-corpus.json", "harness/seek-slice.json", ".research/seek-fixtures"),
    ("harness/colour-corpus.json", "harness/colour-slice.json", ".research/colour-fixtures"),
    ("harness/catalogue-corpus.json", "harness/catalogue-slice.json", ".research/catalogue-fixtures"),
]
FATE = [("mkv/xiph_lacing.mka", True), ("mkv/zero_length_block.mks", False),
        ("mkv/subtitle_zlib.mks", True), ("mkv/codec_delay_opus.mkv", True)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/media-worker")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--compare", type=Path, help="earlier record that must match exactly")
    args = parser.parse_args()
    candidate = args.candidate.resolve()
    identity = load(ROOT / "target/harness/identity.json")
    calls, verdicts = [], {}
    original = runner.execute

    def execute(argv, timeout=30):
        if Path(str(argv[0])).resolve() != candidate:
            return original(argv, timeout)
        result = subprocess.run(list(map(str, argv)), capture_output=True, timeout=timeout, check=False)
        shown = [Path(str(a)).name if i == 2 else str(a) for i, a in enumerate(argv[1:], 1)]
        calls.append({"argv": shown, "exit": result.returncode,
                      "stdout_sha256": digest_bytes(result.stdout)})
        if result.returncode:
            return {"status": "worker-error", "exit_code": result.returncode}
        try:
            return {"status": "ok", "data": json.loads(result.stdout), "process_elapsed_ns": 0}
        except ValueError:
            return {"status": "invalid-worker-output"}

    runner.execute = execute
    started = time.monotonic()
    with tempfile.TemporaryDirectory() as scratch:
        raw = Path(scratch)
        for manifest, contract_path, corpus in SLICES:
            contract = load(ROOT / contract_path)
            for case in load(ROOT / manifest)["cases"]:
                rule = rule_for(case, identity, contract["id"])
                reference = ROOT / reference_record(identity, rule)["path"]
                path = ROOT / corpus / case["file"]
                if digest(path) != case["sha256"]:
                    raise SystemExit(f"input changed: {path}")
                seek = case.get("seek_target_ns")
                operation = (runner.catalogue_parity if contract["id"] == "K5-segment-metadata-v1"
                             else runner.colour_parity if contract["id"] == "K4-colour-hdr-v1"
                             else runner.parity if seek is None else runner.seek_parity)
                result = operation(candidate, reference, path, raw / case["id"], comparison=rule,
                                   native_reference=ROOT / "target/harness/ffmpeg-worker",
                                   source_content=contract["id"] == "K2-bounded-zlib-v1")
                verdicts[contract["id"] + "/" + case["id"]] = result["status"]
        hdr = load(ROOT / "harness/catalogue-slice.json")["required_additional_vectors"][0]
        path = ROOT / ".research/fate-samples" / hdr["file"]
        if digest(path) != hdr["sha256"]:
            raise SystemExit("HDR vector changed")
        verdicts["hdr/" + hdr["file"]] = runner.catalogue_parity(
            candidate, ROOT / "target/harness/ffmpeg-worker", path, raw / "hdr")["status"]
        for sample, sequential in FATE:
            path = ROOT / ".research/fate-samples" / sample
            verdicts["fate/" + sample] = runner.parity(
                candidate, ROOT / "target/harness/ffmpeg-worker", path, raw / Path(sample).stem,
                sequential_required=sequential)["status"]
    record = {"schema": 1, "candidate_sha256": digest(candidate), "verdicts": verdicts,
              "calls": calls, "elapsed_s": round(time.monotonic() - started, 1)}
    write_json(args.output, record, indent=1)
    passed = sum(v == "pass" for v in verdicts.values())
    print(f"{len(verdicts)} cases, {passed} pass, {len(calls)} candidate traces")
    if args.compare:
        before = load(args.compare)
        changed = [b["argv"] for a, b in zip(before["calls"], calls) if a != b]
        if before["verdicts"] != verdicts or len(before["calls"]) != len(calls) or changed:
            print("NOT EQUIVALENT", changed[:10])
            return 1
        print("byte-identical to", args.compare)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
