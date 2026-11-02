#!/usr/bin/env python3
"""Execute one explicitly supported upstream case in an isolated pinned source tree.

The public vector remains in the ignored cache. This is local testing, not permission
to redistribute the sample or qualification of our implementation.
"""
import hashlib
import argparse
import json
from pathlib import Path
import subprocess
import sys
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.runner import parity, digest
from harness.provenance import source_identity_error
CASES={"fate-matroska-xiph-lacing":("mkv/xiph_lacing.mka","xiph",True),"fate-matroska-zero-length-block":("mkv/zero_length_block.mks","zero-length",False),"fate-matroska-zlib-decompression":("mkv/subtitle_zlib.mks","zlib",True)}


def main():
    parser=argparse.ArgumentParser(__doc__)
    parser.add_argument("--case",choices=CASES,default="fate-matroska-xiph-lacing")
    parser.add_argument("--demux-only",action="store_true")
    args=parser.parse_args()
    case=args.case
    sample,label,sequential=CASES[case]
    cache = ROOT / ".research/reference"
    build = json.loads((cache / "build.json").read_text())
    enabled = (cache / "fate-list.txt").read_text().split()
    if not args.demux_only and case not in enabled:
        raise SystemExit("Required configured FATE case disabled; rebuild the reference")
    path = ROOT / ".research/fate-samples" / sample
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        request = urllib.request.Request("https://fate-suite.ffmpeg.org/" + sample, headers={"User-Agent": "triergon-local-fate-check"})
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read(8 * 1024 * 1024 + 1)
        if len(data) > 8 * 1024 * 1024:
            raise ValueError("FATE sample size limit")
        path.write_bytes(data)
    sha = hashlib.sha256(path.read_bytes()).hexdigest()
    receipt_path = cache / ("fate-"+label+".json")
    if receipt_path.exists() and json.loads(receipt_path.read_text())["sample_sha256"] != sha:
        raise ValueError("FATE input changed since previous run")
    source = cache / ("FFmpeg-" + build["revision"])
    result = subprocess.CompletedProcess([],0,"","") if args.demux_only else subprocess.run(["make", "-j2", case, "SAMPLES=" + str(path.parents[1])], cwd=source, capture_output=True, text=True, timeout=180)
    receipt = {"schema": 1, "case": case, "revision": build["revision"], "sample": sample, "sample_sha256": sha,
               "origin": "https://fate-suite.ffmpeg.org/" + sample, "rights": "public external vector; redistribution unreviewed",
               "reference_status": "not-run-demux-only" if args.demux_only else "pass" if result.returncode == 0 else "fail", "upstream_case_executed":not args.demux_only,"exit_code": result.returncode,
               "stdout": result.stdout, "stderr": result.stderr, "candidate_status": "not tested by upstream FATE",
               "upstream_ffmpeg_sha256": digest(source / "ffmpeg"), "reference_build": build}
    candidate = ROOT / "target/release/media-worker"
    reference = ROOT / "target/harness/ffmpeg-worker"
    if candidate.exists() and reference.exists():
        identity = json.loads((ROOT / "target/harness/identity.json").read_text())
        if source_identity_error(ROOT,identity) or identity["binaries"] != {"candidate": digest(candidate), "reference": digest(reference)}:
            raise ValueError("Stale adapter identity; rebuild before evaluating FATE vectors")
        receipt["candidate_demux_comparison"] = parity(candidate, reference, path, ROOT / (".research/fate-candidate-"+label),sequential_required=sequential)
        receipt["candidate_status"] = "demux portion " + receipt["candidate_demux_comparison"]["status"] + "; not upstream framecrc execution"
        receipt["adapter_identity"] = identity
        if source_identity_error(ROOT,identity) or identity["binaries"] != {"candidate": digest(candidate), "reference": digest(reference)}:
            raise ValueError("Adapter/source changed during FATE comparison")
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")
    print(receipt["reference_status"], receipt_path)
    return result.returncode or (1 if receipt.get("candidate_demux_comparison", {}).get("status", "pass") != "pass" else 0)


if __name__ == "__main__":
    raise SystemExit(main())
