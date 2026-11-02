#!/usr/bin/env python3
"""Build the pinned external FFmpeg reference and configure-expanded FATE list.

No sample download, upstream test script execution or production dependency.
"""
import argparse
import hashlib
import json
import re
from pathlib import Path
import subprocess
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
REVISION = "d32b387f2b0a484599d4587d651891f0c63c4238"
ARCHIVE_SHA256 = "8a830a34bfaf98514b5d45cf6c01b1fe78b38d5e4c10eab0de2531b783c15f90"


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--jobs", type=int, default=2)
    a = p.parse_args()
    if not 1 <= a.jobs <= 8:
        p.error("jobs must be 1..8")
    cache = ROOT / ".research/reference"
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / (REVISION + ".tar.gz")
    if not archive.exists():
        request = urllib.request.Request("https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/" + REVISION, headers={"User-Agent": "safe-media-reference-builder"})
        temporary = archive.with_suffix(".partial")
        try:
            with urllib.request.urlopen(request, timeout=60) as source, temporary.open("wb") as target:
                total = 0
                while block := source.read(1024 * 1024):
                    total += len(block)
                    if total > 64 * 1024 * 1024:
                        raise ValueError("source archive size limit")
                    target.write(block)
            temporary.replace(archive)
        finally:
            temporary.unlink(missing_ok=True)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != ARCHIVE_SHA256:
        raise ValueError("Pinned source archive hash mismatch")
    source = cache / ("FFmpeg-" + REVISION)
    if not source.exists():
        with tarfile.open(archive) as bundle:
            bundle.extractall(cache, filter="data")
    # Configured/generated files may exist; every upstream regular source file must
    # still match the immutable archive rather than merely carrying a revision label.
    with tarfile.open(archive) as bundle:
        for member in bundle:
            if not member.isfile():
                continue
            upstream = bundle.extractfile(member)
            path = cache / member.name
            if upstream is None or not path.is_file() or hashlib.sha256(upstream.read()).digest() != hashlib.sha256(path.read_bytes()).digest():
                raise ValueError("Pinned source was changed: " + member.name)
    prefix = cache / "install"
    configure = ["./configure", "--prefix=" + str(prefix), "--disable-everything", "--disable-autodetect", "--disable-doc", "--disable-debug",
                 "--enable-protocol=file,pipe", "--enable-demuxer=matroska", "--enable-muxer=framecrc", "--enable-decoder=pcm_s16le", "--enable-encoder=pcm_s16le,rawvideo",
                 "--enable-parser=h264,hevc,aac,ac3,vorbis", "--enable-ffmpeg", "--enable-ffprobe", "--enable-pic"]
    configuration_record = cache / "configure-argv.json"
    if not (source / "config.h").exists() or not configuration_record.exists() or json.loads(configuration_record.read_text()) != configure:
        subprocess.run(configure, cwd=source, check=True)
        configuration_record.write_text(json.dumps(configure))
    subprocess.run(["make", "-j" + str(a.jobs)], cwd=source, check=True)
    subprocess.run(["make", "install"], cwd=source, check=True)
    listing = subprocess.check_output(["make", "-s", "fate-list", "SAMPLES=" + str(ROOT / ".research/fate-samples")], cwd=source, text=True)
    fate_list = "\n".join(sorted(set(re.findall(r"^fate-[\w.-]+$", listing, re.M)))) + "\n"
    (cache / "fate-list.txt").write_text(fate_list)
    manifest = {"schema": 1, "revision": REVISION, "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(), "configure_argv": configure,
                "fate_list_sha256": hashlib.sha256(fate_list.encode()).hexdigest(), "fate_enabled_count": len(fate_list.split()),
                "note": "Configured case identities only; upstream FATE cases have not been executed."}
    (cache / "build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(cache / "build.json")


if __name__ == "__main__":
    main()
