#!/usr/bin/env python3
"""Build isolated adapters and record exact installed-reference provenance."""
import ctypes
import hashlib
import json
import os
import platform
from pathlib import Path
import shlex
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from harness.provenance import cargo_metadata, matroska_inputs, selected_graph


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def system_libraries(worker):
    if platform.system()!="Darwin":return {"status":"platform-not-collected"}
    listing=subprocess.check_output(["otool","-L",str(worker)],text=True)
    library=next((line.strip().split(" (",1)[0] for line in listing.splitlines()[1:] if "/libz." in line),None)
    info={"status":"recorded","dynamic_links":listing,"os_build":subprocess.check_output(["sw_vers","-buildVersion"],text=True).strip()}
    if library:
        zlib=ctypes.CDLL(library);version=zlib.zlibVersion;version.argtypes=[];version.restype=ctypes.c_char_p
        info["system_zlib"]={"path":library,"version":version().decode("ascii"),"provenance":"OS shared-cache component identified by path, runtime version and OS build; not a standalone content hash"}
    return info


def main():
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    candidate_command = ["cargo", "+nightly", "build", "-p", "media-worker", "--release", "--locked"]
    subprocess.run(candidate_command, cwd=ROOT, env=env, check=True)
    out = ROOT / "target/harness"
    out.mkdir(parents=True, exist_ok=True)
    pinned = ROOT / ".research/reference/build.json"
    if pinned.exists():
        env["PKG_CONFIG_LIBDIR"] = str(ROOT / ".research/reference/install/lib/pkgconfig")
    flags = subprocess.check_output(["pkg-config", "--static", "--cflags", "--libs", "libavformat", "libavcodec", "libavutil"], env=env, text=True)
    command = [os.environ.get("CC", "cc"), "-O3", "-Wall", "-Wextra", "-Werror", str(ROOT / "harness/reference.c"), *shlex.split(flags), "-o", str(out / "ffmpeg-worker")]
    subprocess.run(command, check=True)
    combined_command = [*command[:1], "-DSAFE_MEDIA_COMBINED_REFERENCE=1", *command[1:]]
    combined_command[-1] = str(out / "ffmpeg-combined-worker")
    subprocess.run(combined_command, check=True)
    if platform.system() == "Darwin":
        subprocess.run([command[0], "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-dynamiclib", ROOT / "harness/allocations.c", "-o", out / "allocations.dylib"], check=True)
        subprocess.run([command[0], "-O0", "-fno-builtin", "-Wall", "-Wextra", "-Werror", ROOT / "harness/memory-check.c", "-o", out / "memory-check"], check=True)
    identity = json.loads(subprocess.check_output([out / "ffmpeg-worker", "identity", "-"], text=True))
    graph = selected_graph(ROOT, cargo_metadata(ROOT), "media-worker", matroska_inputs(ROOT))
    identity.update({
        "schema": 2, "reference_kind": "pinned local source build" if pinned.exists() else "installed development reference; source build not yet qualified",
        "candidate_command": candidate_command,
        "selected_graph": graph,
        "build_environment": {key: value for key, value in os.environ.items() if key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "CARGO_BUILD_TARGET") or key.startswith(("CARGO_PROFILE_", "CARGO_TARGET_"))},
        "source_revision": json.loads(pinned.read_text())["revision"] if pinned.exists() else None, "compiler_command": command,
        "comparison_rules": json.loads((ROOT / "harness/comparison-rules.json").read_text()),
        "comparison_references": {
            "ffmpeg-native": {"path": "target/harness/ffmpeg-worker", "sha256": sha(out / "ffmpeg-worker"), "kind": "pinned native FFmpeg"},
            "ffmpeg-combined-native": {"path": "target/harness/ffmpeg-combined-worker", "sha256": sha(out / "ffmpeg-combined-worker"), "kind": "pinned FFmpeg plus native scope-1 combined-encoding restoration", "compiler_command": combined_command,
                "worker_identity": json.loads(subprocess.check_output([out / "ffmpeg-combined-worker", "identity", "-"], text=True)), "system_libraries": system_libraries(out / "ffmpeg-combined-worker")},
        },
        "system_libraries": system_libraries(out/"ffmpeg-worker"),
        "instrumentation": {"observer_sha256": sha(out / "allocations.dylib"), "probe_sha256": sha(out / "memory-check")} if platform.system() == "Darwin" else None,
        "compiler": subprocess.check_output([command[0], "--version"], text=True),
        "ffmpeg_version": subprocess.check_output(["ffmpeg", "-version"], text=True),
        "rustc": subprocess.check_output(["rustc", "+nightly", "-Vv"], text=True),
        "binaries": {"candidate": sha(ROOT / "target/release/media-worker"), "reference": sha(out / "ffmpeg-worker")},
        "source_hashes": {name: digest for package in graph["packages"] for name, digest in package["files"].items()},
        "build_input_hashes": graph["shared_files"],
        "workspace_provenance": {"manifest_sha256": sha(ROOT / "Cargo.toml"), "lock_sha256": sha(ROOT / "Cargo.lock")},
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
