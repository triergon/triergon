#!/usr/bin/env python3
"""Build isolated adapters and record exact installed-reference provenance."""
import hashlib
import json
import os
import platform
from pathlib import Path
import shlex
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    subprocess.run(["cargo", "+nightly", "build", "--workspace", "--release", "--locked"], cwd=ROOT, env=env, check=True)
    out = ROOT / "target/harness"
    out.mkdir(parents=True, exist_ok=True)
    pinned = ROOT / ".research/reference/build.json"
    if pinned.exists():
        env["PKG_CONFIG_LIBDIR"] = str(ROOT / ".research/reference/install/lib/pkgconfig")
    flags = subprocess.check_output(["pkg-config", "--static", "--cflags", "--libs", "libavformat", "libavcodec", "libavutil"], env=env, text=True)
    command = [os.environ.get("CC", "cc"), "-O3", "-Wall", "-Wextra", "-Werror", str(ROOT / "harness/reference.c"), *shlex.split(flags), "-o", str(out / "ffmpeg-worker")]
    subprocess.run(command, check=True)
    if platform.system() == "Darwin":
        subprocess.run([command[0], "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-dynamiclib", ROOT / "harness/allocations.c", "-o", out / "allocations.dylib"], check=True)
        subprocess.run([command[0], "-O0", "-fno-builtin", "-Wall", "-Wextra", "-Werror", ROOT / "harness/memory-check.c", "-o", out / "memory-check"], check=True)
    identity = json.loads(subprocess.check_output([out / "ffmpeg-worker", "identity", "-"], text=True))
    identity.update({
        "schema": 1, "reference_kind": "pinned local source build" if pinned.exists() else "installed development reference; source build not yet qualified",
        "source_revision": json.loads(pinned.read_text())["revision"] if pinned.exists() else None, "compiler_command": command,
        "compiler": subprocess.check_output([command[0], "--version"], text=True),
        "ffmpeg_version": subprocess.check_output(["ffmpeg", "-version"], text=True),
        "rustc": subprocess.check_output(["rustc", "+nightly", "-Vv"], text=True),
        "binaries": {"candidate": sha(ROOT / "target/release/media-worker"), "reference": sha(out / "ffmpeg-worker")},
        "source_hashes": {str(p.relative_to(ROOT)): sha(p) for p in sorted(ROOT.glob("crates/**/src/*.rs"))},
        "build_input_hashes": {str(p.relative_to(ROOT)): sha(p) for p in sorted({ROOT/"Cargo.toml", ROOT/"Cargo.lock", ROOT/"specs/sources.json", *ROOT.glob("harness/*.c"), *ROOT.glob("harness/*.py"), *ROOT.glob("harness/*.json"), *ROOT.glob("tools/*.py"), *ROOT.glob("crates/**/Cargo.toml")})},
        "reference_archives": {p.name:sha(p) for p in sorted((ROOT/".research/reference/install/lib").glob("*.a"))} if pinned.exists() else {},
        "reference_build": json.loads(pinned.read_text()) if pinned.exists() else None,
        "reference_source_sha256": sha(ROOT / "harness/reference.c"),
        "lock_sha256": sha(ROOT / "Cargo.lock"),
        "ffprobe_sha256": sha(shutil.which("ffprobe")),
    })
    (out / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    print(out / "identity.json")


if __name__ == "__main__":
    main()
