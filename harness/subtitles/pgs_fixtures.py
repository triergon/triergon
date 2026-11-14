#!/usr/bin/env python3
"""Recorded receiver fixtures for `crates/subtitles/tests/pgs_receiver.rs`.

For each selected PGS corpus case (written by `pgs_streams.py`), records the
reference worker's prepared packets (`sub-input`) and its decoded subtitles
(`sub-trace`) in a line format the test reads without a JSON parser:

    options forced_only=0|1
    packet PTS|- DTS|- HEX            one per prepared packet, in order
    refused PACKET                     the reference's decode call failed
    event PACKET PTS|- RECTS           a subtitle, then RECTS lines:
    rect X Y W H FLAGS PALETTE PIXELS  palette as 256 ARGB words, hex

Expected values come only from the reference (`target/harness/ffmpeg-subtitles-worker`).

usage: pgs_fixtures.py
"""

import json
import os
import subprocess

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
REFERENCE = os.path.join(ROOT, "target/harness/ffmpeg-subtitles-worker")
CORPUS = os.path.join(ROOT, ".research/subtitle-corpus")
TARGET = os.path.join(ROOT, "crates/subtitles/tests/pgs-receiver-fixtures")
LIMIT = 64 * 1024

CASES = [
    "rx-short-packets-ts", "rx-tail-partial-header-ts", "rx-tail-overrun-ts", "rx-pcs-reads-on-ts",
    "rx-pcs-truncated-sizes-ts", "rx-pds-reads-on-ts", "rx-ods-headers-ts", "rx-displays-ts",
    "rx-forced-empty-rects-ts", "rx-forced-empty-rects-only-ts", "rx-presentation-after-display-ts",
    "rx-epoch-states-ts", "rx-rejected-canvas-ts", "rx-ods-declared-length-ts",
    "rx-clock-before-composition-ts", "rx-rle-plane-ts", "many-objects-sup", "canvas-sizes-sup",
    "rx-canvas-bounds-ts", "rx-ods-first-fragment-ts",
]


def lines(mode, path, arguments):
    out = subprocess.run([REFERENCE, mode, path, *arguments], capture_output=True, text=True, check=True)
    return [json.loads(line) for line in out.stdout.splitlines()[1:]]


def main():
    with open(os.path.join(ROOT, "harness/features/subtitle-pgs/corpus.json")) as f:
        manifest = {case["id"]: case for case in json.load(f)["cases"]}
    os.makedirs(TARGET, exist_ok=True)
    for name in CASES:
        case = manifest[name]
        path = os.path.join(CORPUS, case["file"])
        arguments = case["arguments"]
        forced = "opt:forced_subs_only=1" in arguments
        out = [f"options forced_only={int(forced)}"]
        value = lambda v: "-" if v is None else str(v)
        for packet in lines("sub-input", path, arguments):
            out.append(f"packet {value(packet['pts'])} {value(packet['dts'])} {packet['data'] or '-'}")
        for event in lines("sub-trace", path, arguments):
            if "error" in event:
                out.append(f"refused {event['packet']}")
                continue
            out.append(f"event {event['packet']} {value(event['pts'])} {len(event['rects'])}")
            for r in event["rects"]:
                out.append(f"rect {r['x']} {r['y']} {r['w']} {r['h']} {r['flags']} "
                           f"{r['palette'] or '-'} {r['pixels'] or '-'}")
        text = "\n".join(out) + "\n"
        if len(text) > LIMIT:
            raise SystemExit(f"{name}: fixture over {LIMIT} bytes")
        with open(os.path.join(TARGET, name + ".txt"), "w") as f:
            f.write(text)
    print(f"{len(CASES)} fixtures in {TARGET}")


if __name__ == "__main__":
    main()
