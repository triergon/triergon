#!/usr/bin/env python3
"""Cache authoritative specification texts and verify the recorded content hashes."""
import argparse
from datetime import date
import hashlib
import json
from pathlib import Path
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
SOURCES = [
    ("rfc8794", "https://www.rfc-editor.org/rfc/rfc8794.txt", "EBML normative publication"),
    ("rfc9559", "https://www.rfc-editor.org/rfc/rfc9559.txt", "Matroska normative publication; updates RFC 8794"),
    ("matroska-schema", "https://raw.githubusercontent.com/ietf-wg-cellar/matroska-specification/f93ab02b845ec2a6a2fe4dce5f4c81ad21c34b4c/ebml_matroska.xml", "Pinned current element schema; differences from RFC require explicit disposition"),
    ("matroska-notes", "https://www.matroska.org/technical/notes.html", "Explanatory timestamp/block/lacing notes; moving page frozen by content hash"),
    ("matroska-codecs", "https://www.matroska.org/technical/codec_specs.html", "Codec carriage mappings; moving page frozen by content hash"),
    ("rfc1950", "https://www.rfc-editor.org/rfc/rfc1950.txt", "zlib wrapper"),
    ("rfc1951", "https://www.rfc-editor.org/rfc/rfc1951.txt", "DEFLATE stored/fixed/dynamic blocks"),
]


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--record", action="store_true", help="explicitly create/update the specification manifest")
    a = p.parse_args()
    cache = ROOT / ".research/specs"
    cache.mkdir(parents=True, exist_ok=True)
    manifest = ROOT / "specs/sources.json"
    expected = {} if a.record else {r["id"]: r for r in json.loads(manifest.read_text())["sources"]}
    if not a.record and set(expected) != {entry[0] for entry in SOURCES}:
        raise ValueError("specification manifest and fetch catalog differ")
    rows = []
    for name, url, role in SOURCES:
        path = cache / name
        if not path.exists():
            request = urllib.request.Request(url, headers={"User-Agent": "safe-media-specification-cache"})
            with urllib.request.urlopen(request, timeout=30) as response:
                data = response.read(4 * 1024 * 1024 + 1)
            if len(data) > 4 * 1024 * 1024:
                raise ValueError("specification size limit")
            path.write_bytes(data)
        data = path.read_bytes()
        sha = hashlib.sha256(data).hexdigest()
        if not a.record and expected[name]["sha256"] != sha:
            raise ValueError(name + ": specification drift; review before updating pin")
        rows.append({"id": name, "url": url, "sha256": sha, "bytes": len(data), "role": role, "cached_path": ".research/specs/" + name})
    if a.record:
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text(json.dumps({"schema": 1, "recorded": date.today().isoformat(), "sources": rows}, indent=2) + "\n")
    print(f"Verified {len(rows)} specification snapshots")


if __name__ == "__main__":
    main()
