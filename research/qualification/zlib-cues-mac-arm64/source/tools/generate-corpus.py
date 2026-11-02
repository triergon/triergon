#!/usr/bin/env python3
"""Materialize and hash original structural fixtures plus synthetic encoded streams."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def vint(value, width=None):
    if width is None:
        width = next(w for w in range(1, 9) if value < (1 << (7 * w)) - 1)
    return ((1 << (7 * width)) | value).to_bytes(width, "big")


def element(identifier, data):
    return identifier.to_bytes((identifier.bit_length() + 7) // 8, "big") + vint(len(data)) + data


def uint(identifier, value):
    return element(identifier, value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big"))


def structural(profile):
    ebml = element(0x1A45DFA3, uint(0x4286, 1) + uint(0x42F7, 1) + uint(0x42F2, 4) + uint(0x42F3, 8) + element(0x4282, b"matroska") + uint(0x4287, 4) + uint(0x4285, 2))
    track = uint(0xD7, 1) + uint(0x73C5, 1) + uint(0x83, 2) + element(0x86, b"A_PCM/INT/LIT")
    track += uint(0x23E383, 1_000_000) + element(0xE1, uint(0x9F, 1) + uint(0x6264, 16) + element(0xB5, bytes.fromhex("40bf400000000000")))
    if profile == "codec-delay":
        track += uint(0x56AA, 2_000_000)
    tracks = element(0x1654AE6B, element(0xAE, track))
    flags = {"xiph-lace": 0x82, "fixed-lace": 0x84, "ebml-lace": 0x86}.get(profile, 0x80)
    relative = -4 if profile == "negative-block-time" else 0
    block = b"\x81" + relative.to_bytes(2, "big", signed=True) + bytes([flags])
    if profile == "xiph-lace":
        block += b"\x02\x02\x02" + bytes(range(6))
    elif profile == "fixed-lace":
        block += b"\x02" + bytes(range(6))
    elif profile == "ebml-lace":
        block += b"\x02\x82\xbf" + bytes(range(6))
    else:
        block += b"\x00\x01"
    if profile in ("blockgroup-reference", "discard-padding"):
        group = element(0xA1, block) + uint(0x9B, 1)
        if profile == "blockgroup-reference":
            group += element(0xFB, b"\xff")
        else:
            group += element(0x75A2, (1_000_000).to_bytes(4, "big", signed=True))
        payload = uint(0xE7, 0) + element(0xA0, group)
    else:
        payload = uint(0xE7, 0) + element(0xA3, block)
    if profile == "unknown-cluster":
        cluster = bytes.fromhex("1f43b675ff") + payload
        cluster += element(0x1F43B675, uint(0xE7, 10) + element(0xA3, block))
    else:
        cluster = element(0x1F43B675, payload)
    return ebml + bytes.fromhex("1853806701ffffffffffffff") + element(0x1549A966, uint(0x2AD7B1, 1_000_000)) + tracks + cluster


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--output", type=Path, default=ROOT / ".research/fixtures")
    p.add_argument("--manifest", type=Path, default=ROOT / "harness/corpus.json")
    a = p.parse_args()
    a.output.mkdir(parents=True, exist_ok=True)
    if not (a.output / "avc-aac-sub.mkv").exists():
        subprocess.run(["python3", ROOT / "tools/generate-fixtures.py", "--output", a.output], check=True)
    cases = [("avc-aac-sub", "avc-aac-sub.mkv"), ("hevc-aac", "hevc-aac.mkv")]
    for codec in ("ac3", "eac3"):
        name = "avc-" + codec
        path = a.output / (name + ".mkv")
        if not path.exists():
            subprocess.run(["ffmpeg", "-v", "error", "-nostdin", "-y", "-i", a.output / "avc-aac-sub.mkv", "-map", "0:v", "-map", "0:a", "-c:v", "copy", "-c:a", codec, "-b:a", "192k", path], check=True, timeout=60)
        cases.append((name, path.name))
    for profile in ("unknown-cluster", "xiph-lace", "fixed-lace", "ebml-lace", "negative-block-time", "blockgroup-reference", "codec-delay", "discard-padding"):
        name = profile + ".mkv"
        (a.output / name).write_bytes(structural(profile))
        cases.append((profile, name))
    manifest = {"schema": 1, "status": "original synthetic corpus, not upstream FATE execution", "generator": "tools/generate-corpus.py", "cases": [
        {"id": profile, "profile": profile, "file": name, "sha256": hashlib.sha256((a.output / name).read_bytes()).hexdigest(), "rights": "authored-synthetic"}
        for profile, name in cases]}
    a.manifest.parent.mkdir(parents=True, exist_ok=True)
    a.manifest.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Materialized {len(cases)} cases: {a.manifest}")


if __name__ == "__main__":
    main()
