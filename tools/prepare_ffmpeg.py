#!/usr/bin/env python3
"""Build the pinned external FFmpeg reference and configure-expanded FATE list.

No sample download, upstream test script execution or production dependency.
"""
import argparse
import json
import re
from pathlib import Path
import subprocess
import sys
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.common import digest, digest_bytes, load, write_json
REVISION = "d32b387f2b0a484599d4587d651891f0c63c4238"
ARCHIVE_SHA256 = "8a830a34bfaf98514b5d45cf6c01b1fe78b38d5e4c10eab0de2531b783c15f90"


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("--jobs", type=int, default=2)
    p.add_argument("--profile", choices=("matroska", "formats"), default="matroska",
                   help="formats builds an isolated MOV/TS/subtitle reference; existing Matroska evidence is untouched")
    a = p.parse_args()
    if not 1 <= a.jobs <= 8:
        p.error("jobs must be 1..8")
    cache = ROOT / (".research/reference" if a.profile == "matroska" else ".research/reference-formats")
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / (REVISION + ".tar.gz")
    prior_archive = ROOT / ".research/reference" / (REVISION + ".tar.gz")
    if a.profile != "matroska" and prior_archive.is_file():
        archive = prior_archive  # read-only reuse of the already pinned source download
    if not archive.exists():
        request = urllib.request.Request("https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/" + REVISION, headers={"User-Agent": "triergon-reference-builder"})
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
    if digest(archive) != ARCHIVE_SHA256:
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
            if upstream is None or not path.is_file() or digest_bytes(upstream.read()) != digest(path):
                raise ValueError("Pinned source was changed: " + member.name)
    prefix = cache / "install"
    configure = ["./configure", "--prefix=" + str(prefix), "--disable-everything", "--disable-autodetect", "--disable-doc", "--disable-debug",
                 "--enable-protocol=file,pipe", "--enable-demuxer=matroska", "--enable-muxer=framecrc", "--enable-decoder=pcm_s16le", "--enable-encoder=pcm_s16le,rawvideo",
                 "--enable-parser=h264,hevc,aac,ac3,vorbis", "--enable-ffmpeg", "--enable-ffprobe", "--enable-pic", "--enable-zlib"]
    if a.profile == "formats":
        replacements = {
            "--enable-demuxer=matroska": "--enable-demuxer=matroska,mov,mpegts,h264,hevc,srt,ass,webvtt,sup,vobsub",
            "--enable-decoder=pcm_s16le": "--enable-decoder=pcm_s16le,pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt",
            "--enable-parser=h264,hevc,aac,ac3,vorbis": "--enable-parser=h264,hevc,aac,ac3,vorbis,dvdsub,dvbsub",
        }
        configure = [replacements.get(flag, flag) for flag in configure]
        configure.append("--enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata")
    configuration_record = cache / "configure-argv.json"
    if not (source / "config.h").exists() or not configuration_record.exists() or load(configuration_record) != configure:
        subprocess.run(configure, cwd=source, check=True)
        configuration_record.write_text(json.dumps(configure))
    subprocess.run(["make", "-j" + str(a.jobs)], cwd=source, check=True)
    subprocess.run(["make", "install"], cwd=source, check=True)
    listing = subprocess.check_output(["make", "-s", "fate-list", "SAMPLES=" + str(ROOT / ".research/fate-samples")], cwd=source, text=True)
    fate_list = "\n".join(sorted(set(re.findall(r"^fate-[\w.-]+$", listing, re.M)))) + "\n"
    (cache / "fate-list.txt").write_text(fate_list)
    manifest = {"schema": 1, "revision": REVISION, "archive_sha256": digest(archive), "configure_argv": configure,
                "fate_list_sha256": digest_bytes(fate_list.encode()), "fate_enabled_count": len(fate_list.split()),
                "note": "Configured case identities only; upstream FATE cases have not been executed."}
    if a.profile != "matroska":
        manifest["profile"] = a.profile
        manifest["reference_archives"] = {str(path.relative_to(cache)): digest(path)
            for path in sorted((prefix / "lib").glob("*.a"))}
        manifest["compiler"] = subprocess.check_output(["cc", "--version"], text=True)
    write_json(cache / "build.json", manifest)
    print(cache / "build.json")


if __name__ == "__main__":
    main()
