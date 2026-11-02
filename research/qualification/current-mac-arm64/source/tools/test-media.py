#!/usr/bin/env python3
"""Run shared compatibility/benchmark workers; acceptance is always explicit."""
import argparse
import json
import os
from pathlib import Path
import platform
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.runner import acceptance, benchmark, digest, fate_coverage, load, memory, parity


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--corpus", type=Path, default=ROOT / ".research/fixtures")
    p.add_argument("--manifest", type=Path, default=ROOT / "harness/corpus.json")
    p.add_argument("--contract",type=Path,default=ROOT/"harness/slice.json")
    p.add_argument("--output", type=Path, default=ROOT / ".research/harness-report")
    p.add_argument("--fate-cache", type=Path)
    p.add_argument("--benchmark", action="store_true")
    p.add_argument("--memory", action="store_true")
    p.add_argument("--pairs", type=int, default=30)
    p.add_argument("--iterations", type=int, default=200)
    p.add_argument("--seed", type=int, default=280)
    p.add_argument("--require-acceptance", action="store_true")
    p.add_argument("--require-slice-acceptance", action="store_true", help="require every current K0 case to pass parity/time/memory; broader module gate remains separate")
    p.add_argument("--reuse-report", type=Path, help="reuse complete measurements only for identical workers, contract and input hashes")
    a = p.parse_args()
    candidate = ROOT / "target/release/media-worker"
    reference = ROOT / "target/harness/ffmpeg-worker"
    identity = load(ROOT / "target/harness/identity.json")
    if identity["binaries"] != {"candidate": digest(candidate), "reference": digest(reference)}:
        p.error("adapter changed since build identity; rebuild")
    sources = {str(path.relative_to(ROOT)): digest(path) for path in sorted(ROOT.glob("crates/**/src/*.rs"))}
    if sources != identity["source_hashes"] or any(digest(ROOT / name) != sha for name, sha in identity["build_input_hashes"].items()):
        p.error("source/configuration changed since build identity; rebuild")
    contract = load(a.contract)
    previous = load(a.reuse_report) if a.reuse_report else None
    reusable = {}
    if previous:
        if previous["identity"]["binaries"] != identity["binaries"] or previous["contract_sha256"] != digest(a.contract):
            p.error("measurement reuse requires identical workers and feature contract")
        reusable = {row["id"]: row for row in previous["cases"]}
    a.output.mkdir(parents=True, exist_ok=True)
    rows = []
    for case in load(a.manifest)["cases"]:
        row = {"id": case["id"], "profile": case["profile"]}
        path = (a.corpus / case["file"]).resolve()
        if not path.is_relative_to(a.corpus.resolve()):
            p.error("input escapes corpus")
        if not path.is_file():
            row["parity"] = {"status": "missing-sample"}
        elif digest(path) != case["sha256"]:
            row["parity"] = {"status": "input-hash-mismatch"}
        elif case.get("rights") != "authored-synthetic":
            row["parity"] = {"status": "rights-unreviewed"}
        else:
            row["input_sha256"] = case["sha256"]
            row["parity"] = parity(candidate, reference, path, a.output / case["id"])
            if row["parity"]["status"] == "pass":
                old = reusable.get(case["id"], {})
                if old.get("input_sha256") != case["sha256"]:
                    old = {}
                if a.benchmark:
                    timing=old.get("timing",{})
                    row["timing"] = timing if timing.get("status")=="measured" and timing.get("pairs")==a.pairs and timing.get("iterations")==a.iterations else benchmark(candidate, reference, path, a.pairs, a.iterations, a.seed)
                if a.memory:
                    measured=old.get("memory",{})
                    row["memory"] = measured if measured.get("status")=="qualified" and measured.get("pairs")==a.pairs else memory(candidate, reference, path, a.pairs, a.seed)
                if old:
                    row["reused_evidence_sha256"]=digest(a.reuse_report)
        print(case["id"], row["parity"]["status"], flush=True)
        rows.append(row)
    target = {("Darwin", "arm64"): "aarch64-apple-darwin", ("Linux", "x86_64"): "x86_64-unknown-linux-gnu"}.get((platform.system(), platform.machine()), "unqualified-" + platform.machine())
    coverage = fate_coverage(load(ROOT / "research/fate-index.json"), load(ROOT / "harness/fate-map.json"), a.fate_cache)
    verdict = acceptance(contract, rows, identity, target, coverage)
    report = {"schema": 1, "target": target, "identity": identity, "contract_sha256": digest(a.contract), "contract_id":contract["id"],
              "corpus_manifest_sha256": digest(a.manifest), "environment": {"platform": platform.platform(), "load": os.getloadavg()},
              "cases": rows, "fate_coverage": coverage, "acceptance": verdict}
    (a.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("ACCEPTED" if verdict["accepted"] else "NOT ACCEPTED: " + "; ".join(verdict["reasons"]))
    print(contract["id"]+" SLICE ACCEPTED" if verdict["slice_accepted"] else contract["id"]+" SLICE NOT ACCEPTED: " + "; ".join(verdict["slice_reasons"]))
    if a.require_slice_acceptance and not verdict["slice_accepted"]:
        return 2
    if a.require_acceptance and not verdict["accepted"]:
        return 2
    return 0 if rows and all(r["parity"]["status"] == "pass" for r in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
