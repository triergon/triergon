#!/usr/bin/env python3
"""Record exact capture-pipeline output identities.

Runs `capture-worker convert`, `recon` and `pipeline` over deterministic RGBA
inputs at the geometry, orientation and bit rate of every case in
harness/capture/cases.json, and records the SHA-256 of each output plus the
exit status and stderr. Refactors of the converter, encoder, multiplexer and
session must reproduce the record exactly. The inputs are generated here in
pure Python (seeded noise, a scrolled texture, a scene cut and a static screen);
they are equivalence inputs only, not the qualification inputs of
harness/capture/generate_inputs.py, and this is never parity or performance
evidence.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import random
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.common import digest, digest_bytes, load, write_json

FRAMES = 40  # one 30-picture GOP plus the start of the next


def texture(width, height, seed):
    """A smooth texture: separable low-frequency ramps with a little noise."""
    rng = random.Random(seed)
    cols = [(x * 7 + rng.randrange(8)) & 0xFF for x in range(width)]
    out = bytearray(width * height * 4)
    for y in range(height):
        base = (y * 3) & 0xFF
        row = bytearray(width * 4)
        row[0::4] = bytes((c + base) & 0xFF for c in cols)
        row[1::4] = bytes((c * 2 + base // 2) & 0xFF for c in cols)
        row[2::4] = bytes((255 - c + base) & 0xFF for c in cols)
        row[3::4] = bytes(rng.randrange(256) for _ in range(width))
        out[y * width * 4:(y + 1) * width * 4] = row
    return bytes(out)


def frames(content, width, height, seed):
    size = width * height * 4
    if content == "noise":
        rng = random.Random(seed)
        return b"".join(rng.randbytes(size) for _ in range(FRAMES))
    if content in ("ui_static", "ui_transition"):
        a = texture(width, height, seed)
        if content == "ui_static":
            return a * FRAMES
        b = texture(width, height, seed + 1)[::-1]
        return a * (FRAMES // 3) + b * (FRAMES - FRAMES // 3)  # a scene cut
    canvas = texture(width, height + 6 * FRAMES, seed)  # ui_scroll and motion
    step = 6 if content == "ui_scroll" else 2
    return b"".join(canvas[k * step * width * 4:k * step * width * 4 + size] for k in range(FRAMES))


def run(job):
    name, argv, output = job
    result = subprocess.run(argv, capture_output=True, timeout=600, check=False)
    entry = {"exit": result.returncode, "stdout_sha256": digest_bytes(result.stdout),
             "stderr_sha256": digest_bytes(result.stderr)}
    if output:
        entry["output_sha256"] = digest(output) if os.path.exists(output) else None
        if os.path.exists(output):
            os.unlink(output)
    return name, entry


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--jobs", type=int, default=os.cpu_count())
    args = parser.parse_args()
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    subprocess.run(["cargo", "build", "--release", "--locked", "--bin", "capture-worker"],
                   cwd=ROOT, env=env, check=True)
    worker = str(ROOT / "target/release/capture-worker")
    record = {}
    with tempfile.TemporaryDirectory() as scratch:
        for index, case in enumerate(load(ROOT / "harness/capture/cases.json")["cases"]):
            w, h, flip, rate = case["width"], case["height"], case["flip"], case["bitrate"]
            path = Path(scratch) / f"{case['id']}.rgba"
            path.write_bytes(frames(case["content"], w, h, 280 + index))
            geometry = [str(w), str(h), str(flip), str(FRAMES)]
            out = lambda kind: str(Path(scratch) / f"{case['id']}.{kind}")
            jobs = [(f"{case['id']}/convert", [worker, "convert", *geometry, str(path), out("yuv")], out("yuv")),
                    (f"{case['id']}/recon", [worker, "recon", *geometry, str(rate), str(path), out("recon")], out("recon")),
                    (f"{case['id']}/pipeline", [worker, "pipeline", *geometry, str(rate), str(path), out("ts")], out("ts"))]
            with ThreadPoolExecutor(args.jobs) as pool:
                record.update(pool.map(run, jobs))
            record[f"{case['id']}/input"] = {"sha256": digest(path)}
            path.unlink()
    record = dict(sorted(record.items()))
    write_json(args.output, {"schema": 1, "frames": FRAMES, "outputs": record}, indent=1)
    print(len(record), "records;", sum(r.get("exit") == 0 for r in record.values()), "exit 0")
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
