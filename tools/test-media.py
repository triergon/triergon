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
from harness.reuse import retained_memory, retained_timing
from harness.comparison import rule_for, reference_record, reference_summary
from harness.runner import acceptance, benchmark, digest, fate_coverage, load, memory, parity, seek_parity, colour_parity, catalogue_parity, measurement_cache, checkpoint_report
from harness.provenance import source_identity_error as provenance_source_error


def save_report(path, report):
    temporary=path.with_suffix(".tmp")
    temporary.write_text(json.dumps(report,indent=2)+"\n")
    temporary.replace(path)


def source_identity_error(identity):
    return provenance_source_error(ROOT, identity)


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
    p.add_argument("--additional-report",type=Path,action="append",default=[],help="exact-source reports for mandatory additional vectors")
    a = p.parse_args()
    candidate = ROOT / "target/release/media-worker"
    native_reference = ROOT / "target/harness/ffmpeg-worker"
    identity = load(ROOT / "target/harness/identity.json")
    if identity["binaries"] != {"candidate": digest(candidate), "reference": digest(native_reference)}:
        p.error("adapter changed since build identity; rebuild")
    for record in identity.get("comparison_references", {}).values():
        worker = (ROOT / record["path"]).resolve()
        if not worker.is_relative_to((ROOT / "target/harness").resolve()) or digest(worker) != record["sha256"]:
            p.error("comparison reference changed since build identity; rebuild")
    if error := source_identity_error(identity):
        p.error(error + "; rebuild")
    contract = load(a.contract)
    additional=[]
    for path in a.additional_report:
        receipt=load(path)
        if receipt.get("identity")!=identity or receipt.get("contract_sha256")!=digest(a.contract):p.error("additional vector receipt has stale source/contract identity")
        additional.append(receipt["case"])
    previous = load(a.reuse_report) if a.reuse_report else None
    previous_sha = digest(a.reuse_report) if a.reuse_report else None
    reusable = {}
    if previous:
        if a.output.resolve() == a.reuse_report.parent.resolve():
            p.error("measurement reuse needs a different output directory to preserve the previous report and raw observations")
        if previous["identity"]["binaries"] != identity["binaries"] or previous["contract_sha256"] != digest(a.contract) or previous["identity"].get("comparison_references") != identity.get("comparison_references"):
            p.error("measurement reuse requires identical workers and feature contract")
        reusable = measurement_cache(previous)
    a.output.mkdir(parents=True, exist_ok=True)
    target = {("Darwin", "arm64"): "aarch64-apple-darwin", ("Linux", "x86_64"): "x86_64-unknown-linux-gnu", ("Linux", "aarch64"): "aarch64-unknown-linux-gnu"}.get((platform.system(), platform.machine()), "unqualified-" + platform.machine())
    coverage = fate_coverage(load(ROOT / "research/fate-index.json"), load(ROOT / "harness/fate-map.json"), a.fate_cache)
    report_base = {"schema":1,"target":target,"identity":identity,"contract_sha256":digest(a.contract),"contract_id":contract["id"],"corpus_manifest_sha256":digest(a.manifest),"environment":{"platform":platform.platform(),"load":os.getloadavg()},"fate_coverage":coverage,"additional_cases":additional}
    rows = []
    for case in load(a.manifest)["cases"]:
        row = {"id": case["id"], "profile": case["profile"]}
        try:
            comparison = rule_for(case, identity, contract["id"])
            selected = reference_record(identity, comparison)
        except ValueError as error:
            p.error(str(error))
        reference = ROOT / selected["path"]
        row["reference"] = reference_summary(selected)
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
            seek = case.get("seek_target_ns")
            catalogue = contract["id"] == "K5-segment-metadata-v1"
            colour = contract["id"] == "K4-colour-hdr-v1"
            operation = ("catalogue-", []) if catalogue else ("metadata-", []) if colour else ("codec-", []) if comparison else None if seek is None else ("seek-",[seek])
            row["operation"] = {"name": "container-demux-v1" if seek is None else "container-seek-preroll-v1", "seek_target_ns": seek,
                                "view": "catalogue-source-and-native-v1" if catalogue else "colour-source-and-ffmpeg9-v1" if colour else comparison["view"] if comparison else "source-frames-ffmpeg9-clock-v1"}
            row["parity"] = (catalogue_parity if catalogue else colour_parity if colour else parity if seek is None else seek_parity)(candidate, reference, path, a.output / case["id"],
                comparison=comparison, native_reference=native_reference, source_content=contract["id"] == "K2-bounded-zlib-v1")
            if row["parity"]["status"] == "pass":
                old = reusable.get(case["id"], {})
                if old.get("input_sha256") != case["sha256"] or old.get("operation") != row["operation"] or old.get("reference") != row["reference"]:
                    old = {}
                if a.benchmark:
                    timing=old.get("timing",{})
                    try:
                        retained = retained_timing(timing, a.pairs, a.iterations, contract["minimum_pairs"])
                    except ValueError as error:
                        p.error(str(error))
                    row["timing"] = retained if retained is not None else benchmark(candidate, reference, path, a.pairs, a.iterations, a.seed, operation)
                if a.memory:
                    measured=old.get("memory",{})
                    try:
                        retained = retained_memory(measured, a.pairs, contract["minimum_pairs"], previous["identity"].get("instrumentation") if previous else None, identity.get("instrumentation"))
                    except ValueError as error:
                        p.error(str(error))
                    row["memory"] = retained if retained is not None else memory(candidate, reference, path, a.pairs, a.seed, operation)
                if old:
                    row["reused_evidence_sha256"]=previous_sha
                    row["previous_measurement_report"]={"path":str(a.reuse_report.resolve()),"sha256":previous_sha}
        print(case["id"], row["parity"]["status"], flush=True)
        rows.append(row)
        # Keep completed cases even if the authorized isolation window expires.
        # A checkpoint is never an acceptance receipt.
        save_report(a.output/"partial.json",checkpoint_report(report_base,rows,reusable))
    verdict=acceptance(contract,rows,identity,target,coverage,additional)
    if error := source_identity_error(identity):
        verdict["accepted"] = verdict["slice_accepted"] = False
        verdict["reasons"].append(error)
        verdict["slice_reasons"].append(error)
    report={**report_base,"collection_status":"complete","cases":rows,"acceptance":verdict}
    save_report(a.output/"report.json",report)
    print("ACCEPTED" if verdict["accepted"] else "NOT ACCEPTED: " + "; ".join(verdict["reasons"]))
    print(contract["id"]+" SLICE ACCEPTED" if verdict["slice_accepted"] else contract["id"]+" SLICE NOT ACCEPTED: " + "; ".join(verdict["slice_reasons"]))
    if a.require_slice_acceptance and not verdict["slice_accepted"]:
        return 2
    if a.require_acceptance and not verdict["accepted"]:
        return 2
    return 0 if rows and all(r["parity"]["status"] == "pass" for r in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
