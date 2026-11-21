#!/usr/bin/env python3
"""Build the authored PGS and DVB palette corpora and optionally record their folds."""

import argparse
import hashlib
import itertools
import json
from pathlib import Path
import struct
import subprocess

if __package__:
    from . import dvb_streams as dvb, pgs_streams as pgs
else:
    import dvb_streams as dvb
    import pgs_streams as pgs


ROOT = Path(__file__).resolve().parents[2]
SOURCE = "harness/subtitles/palette_sweep.py"


def pgs_colours():
    for axis, channel in enumerate(("y", "cr", "cb")):
        for first, second in itertools.product((16, 128, 235), repeat=2):
            fixed = [first, second]
            yield f"{channel}-{first}-{second}", [
                tuple(fixed[:axis] + [value] + fixed[axis:]) for value in range(256)
            ]
    for y_index, y in enumerate(range(0, 256, 17)):
        for band in (0, 1):
            yield f"cube-{y_index:02}-{band}", list(itertools.product(
                (y,), range(band * 136, (band + 1) * 136, 17), range(0, 256, 17)
            ))


def pgs_cases():
    for width, height in ((720, 480), (720, 576), (1280, 720), (1920, 1080)):
        for sweep, colours in pgs_colours():
            count = len(colours)
            indices = list(range(count))
            palette = [(i, *colour, 255 - i) for i, colour in enumerate(colours)]
            payload = b"".join([
                pgs.pcs(width, height, 0, 0x80, [(0, 0, 0, 0, False, None)]),
                pgs.wds([(0, 0, 0, count, 1)]),
                pgs.pds(0, 0, palette),
                *pgs.ods_parts(0, 0, count, 1, pgs.rle([indices])),
                pgs.END,
            ])
            header = b"SMBMO001" + struct.pack(">BIIIII", 1, 0, 1, 90_000, 0, 1)
            packet = struct.pack(">BBqBqI", 0, 1, 90_000, 1, 90_000, len(payload))
            yield f"pgs-{height}-{sweep}", "case", header + packet + payload, [], (1, count)


def dvb_colour(index, full):
    # Odd strides visit distinct words in each power-of-two wire cube.
    if full:
        word = (index * 0x4d19cb) & 0xffffff
        return word >> 16, word >> 8 & 255, word & 255, index & 255
    word = (index * 0x26bb) & 0x3fff
    return word >> 8, word >> 4 & 15, word & 15, index & 3


def dvb_cases():
    for depth, sets_per_stream in ((2, 16), (4, 8), (8, 2)):
        count = 1 << depth
        indices = list(range(count))
        pixels = dvb.obj(1, depth, [indices, indices])
        for full in (True, False):
            form = "full" if full else "reduced"
            for stream in range(20):
                display_sets = []
                for step in range(sets_per_stream):
                    start = (stream * sets_per_stream + step) * count
                    entries = [(i, 1 << (3 - dvb.DEPTH_CODE[depth]),
                                *dvb_colour(start + i, full), full) for i in indices]
                    data = b"".join([
                        dvb.dds(),
                        dvb.pcs(state=2, regions=[(0, 0, 0)]),
                        dvb.rcs(0, count, 2, depth=depth, objects=[(1, 0, 0)]),
                        dvb.cds(0, entries), pixels, dvb.eds(),
                    ])
                    display_sets.append((90_000 + step * 90_000, data))
                # Each rectangle contributes four bytes per palette entry and
                # two rows of indices.
                work = (sets_per_stream, sets_per_stream * (4 + 2) * count)
                yield f"dvb-{depth}bit-{form}-{stream:02}", "ts", dvb.single(display_sets), ["dvb"], work


def build(output_root, feature, cases, reference, operation):
    directory = output_root / ".research" / feature
    directory.mkdir(parents=True, exist_ok=True)
    rows = []
    for case, extension, data, arguments, work in cases:
        path = directory / f"{case}.{extension}"
        if not path.exists() or path.read_bytes() != data:
            path.write_bytes(data)
        if reference is None:
            continue
        stdout = subprocess.run([reference, operation, path, *arguments],
                                check=True, capture_output=True, text=True).stdout
        fold = json.loads(stdout.splitlines()[-1])
        if not fold["eof"] or (fold["packets"], fold["bytes"]) != work:
            raise ValueError(f"{case}: expected {work} rectangles and bytes: {fold}")
        rows.append({
            "id": case, "profile": case, "file": path.name,
            "sha256": hashlib.sha256(data).hexdigest(), "rights": "authored-synthetic",
            "source": SOURCE, "arguments": arguments,
            "expected": {field: fold[field] for field in ("packets", "bytes", "digest")},
        })
    if reference is not None:
        manifest = output_root / "harness" / "features" / feature / "corpus.json"
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text(json.dumps({"schema": 1, "cases": rows}, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-root", type=Path, default=ROOT, help="repository layout to populate")
    parser.add_argument("--record", action="store_true", help="record pinned reference folds")
    options = parser.parse_args()
    workers = ROOT / "target" / "harness"
    for feature, cases, worker, operation in [
        ("pgs-palette-sweep", pgs_cases, "ffmpeg-formats-worker", "pgs-fold"),
        ("dvb-palette-sweep", dvb_cases, "ffmpeg-subtitles-worker", "sub-fold"),
    ]:
        build(options.output_root, feature, cases(), workers / worker if options.record else None, operation)


if __name__ == "__main__":
    main()
