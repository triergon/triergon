#!/usr/bin/env python3
"""Render a qualification run's per-case results as a Markdown table."""
import json
import sys
from pathlib import Path


def row(r):
    c = r["correctness_summary"]
    rt = r["runtime"]
    m = r["memory"]
    v = r["verdict"]
    heap = m.get("heap_peak_bytes", {})
    heap_cell = "invalid"
    if heap:
        heap_cell = f"{max(heap['candidate']) / 1e6:.2f} / {min(heap['reference']) / 1e6:.2f}"
    rss = m.get("rss_bound_ratio")
    streem = r.get("runtime_vs_streem_config", {}).get("ratio", {})
    return "| {id} | {conv} | {mux} | {enc} ({mode}, {bits}×, {pc:.2f}/{pr:.2f} dB) | {rmed:.3f} / **{rup:.3f}**{tgt} | {smed} | {heap} | {rss} | {all} |".format(
        id=r["case"]["id"],
        conv=f"{'pass' if c.get('conversion') else 'FAIL'} (±{c.get('conversion_max_abs')}, {100 * c.get('conversion_exact', 0):.2f}% exact)",
        mux="pass" if c.get("mux") else "FAIL",
        enc="pass" if c.get("encoder") else "FAIL",
        mode=c.get("encoder_mode"),
        bits=c.get("bits_ratio"),
        pc=c.get("interpolated_psnr_y") or c["psnr_y"][0],
        pr=c["psnr_y"][1],
        rmed=rt["ratio"]["median"],
        rup=rt["ratio"]["upper_95"],
        tgt=" ✓0.80" if rt["target_pass"] else "",
        smed=f"{streem['median']:.3f}" if streem else "–",
        heap=heap_cell + (" ✓" if m["heap_gate_pass"] else " ✗"),
        rss=(f"{rss['upper_95']:.3f}" if rss else "n/a") + (" ✓" if m["rss_gate_pass"] else " ✗"),
        all="**pass**" if v["all_gates"] else "**FAIL**",
    )


def main():
    run = Path(sys.argv[1])
    results = sorted(run.glob("*/result.json"))
    print("| Case | Conversion | Mux | Encoder (mode, bits vs FFmpeg, PSNR-Y ours/FFmpeg) | Runtime ratio median / upper95 | vs Streem build (median) | Heap peak MB ours / FFmpeg | RSS bound upper95 | All gates |")
    print("|---|---|---|---|---|---|---|---|---|")
    for path in results:
        print(row(json.loads(path.read_text())))
    return 0


if __name__ == "__main__":
    sys.exit(main())
