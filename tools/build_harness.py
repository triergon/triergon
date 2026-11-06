#!/usr/bin/env python3
"""Build isolated adapters and record exact installed-reference provenance."""
import ctypes
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
from harness.common import digest as sha, load, write_json
from harness.provenance import cargo_metadata, matroska_inputs, selected_graph
from harness.memory import linux_linked_libraries, linux_library_identity, linked_hashes, kernel_profile_current


def system_libraries(worker):
    if platform.system() == "Linux":
        return linux_library_identity(linux_linked_libraries(worker))
    if platform.system()!="Darwin":return {"status":"platform-not-collected"}
    listing=subprocess.check_output(["otool","-L",str(worker)],text=True)
    library=next((line.strip().split(" (",1)[0] for line in listing.splitlines()[1:] if "/libz." in line),None)
    info={"status":"recorded","dynamic_links":listing,"os_build":subprocess.check_output(["sw_vers","-buildVersion"],text=True).strip()}
    if library:
        zlib=ctypes.CDLL(library);version=zlib.zlibVersion;version.argtypes=[];version.restype=ctypes.c_char_p
        info["system_zlib"]={"path":library,"version":version().decode("ascii"),"provenance":"OS shared-cache component identified by path, runtime version and OS build; not a standalone content hash"}
    return info


def instrumentation_commands(compiler, out, system):
    if system == "Darwin":
        return [[compiler, "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-dynamiclib", ROOT / "harness/allocations.c", "-o", out / "allocations.dylib"],
                [compiler, "-O0", "-fno-builtin", "-Wall", "-Wextra", "-Werror", ROOT / "harness/memory-check.c", "-o", out / "memory-check"]]
    if system == "Linux":
        return [[compiler, "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-fno-builtin", "-Wl,-z,now", "-shared", "-fPIC", ROOT / "harness/allocations-linux.c", "-o", out / "allocations.so", "-ldl", "-pthread"],
                [compiler, "-std=c11", "-O0", "-fno-builtin", "-Wall", "-Wextra", "-Werror", ROOT / "harness/portable-memory-check.c", "-o", out / "memory-check", "-ldl", "-pthread"]]
    raise ValueError("memory instrumentation unsupported on " + system)


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
    system = platform.system()
    commands = instrumentation_commands(command[0], out, system)
    for compile_command in commands:
        subprocess.run(compile_command, check=True)
    instrumentation = {"observer_sha256": sha(out / ("allocations.so" if system == "Linux" else "allocations.dylib")), "probe_sha256": sha(out / "memory-check")}
    libraries = {}
    if system == "Linux":
        for name, worker in (("candidate", ROOT / "target/release/media-worker"), ("reference", out / "ffmpeg-worker"), ("combined_reference", out / "ffmpeg-combined-worker"), ("probe", out / "memory-check")):
            libraries[name] = linux_linked_libraries(worker)
        libraries["observer"] = linux_linked_libraries(out / "allocations.so", libraries["probe"]["interpreter"])
        instrumentation.update(platform="Linux", architecture=platform.machine(), schema=2,
            observer_path="target/harness/allocations.so", preload_variable="LD_PRELOAD",
            linked_library_hashes=linked_hashes({name: libraries[name] for name in ("observer", "probe")}),
            binding_environment=libraries["probe"]["binding_environment"],
            compiler_commands=[[str(arg) for arg in argv] for argv in commands],
            source_hashes={"harness/allocations-linux.c": sha(ROOT / "harness/allocations-linux.c"),
                           "harness/portable-memory-check.c": sha(ROOT / "harness/portable-memory-check.c")})
        proof_path = os.environ.get("SAFE_MEDIA_RSS_KERNEL_PROOF")
        if proof_path:
            proof_file = ROOT / proof_path
            profile = {"proof_path": proof_path, "proof_sha256": sha(proof_file),
                       "proof": load(proof_file)}
            if not kernel_profile_current(profile, instrumentation["observer_sha256"], instrumentation["probe_sha256"]):
                raise ValueError("requested kernel RSS proof is unavailable or stale")
            instrumentation["rss_kernel_bound"] = {key: profile[key] for key in ("proof_path", "proof_sha256")}
    identity = json.loads(subprocess.check_output([out / "ffmpeg-worker", "identity", "-"], text=True))
    graph = selected_graph(ROOT, cargo_metadata(ROOT), "media-worker", matroska_inputs(ROOT))
    identity.update({
        "schema": 2, "reference_kind": "pinned local source build" if pinned.exists() else "installed development reference; source build not yet qualified",
        "candidate_command": candidate_command,
        "selected_graph": graph,
        "build_environment": {key: value for key, value in os.environ.items() if key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "CARGO_BUILD_TARGET") or key.startswith(("CARGO_PROFILE_", "CARGO_TARGET_"))},
        "source_revision": load(pinned)["revision"] if pinned.exists() else None, "compiler_command": command,
        "comparison_rules": load(ROOT / "harness/comparison-rules.json"),
        "comparison_references": {
            "ffmpeg-native": {"path": "target/harness/ffmpeg-worker", "sha256": sha(out / "ffmpeg-worker"), "kind": "pinned native FFmpeg"},
            "ffmpeg-combined-native": {"path": "target/harness/ffmpeg-combined-worker", "sha256": sha(out / "ffmpeg-combined-worker"), "kind": "pinned FFmpeg plus native scope-1 combined-encoding restoration", "compiler_command": combined_command,
                "worker_identity": json.loads(subprocess.check_output([out / "ffmpeg-combined-worker", "identity", "-"], text=True)), "system_libraries": linux_library_identity(libraries["combined_reference"]) if system == "Linux" else system_libraries(out / "ffmpeg-combined-worker")},
        },
        "system_libraries": linux_library_identity(libraries["reference"]) if system == "Linux" else system_libraries(out/"ffmpeg-worker"),
        "instrumentation": instrumentation,
        "compiler": subprocess.check_output([command[0], "--version"], text=True),
        "ffmpeg_version": subprocess.check_output(["ffmpeg", "-version"], text=True),
        "rustc": subprocess.check_output(["rustc", "+nightly", "-Vv"], text=True),
        "binaries": {"candidate": sha(ROOT / "target/release/media-worker"), "reference": sha(out / "ffmpeg-worker")},
        "source_hashes": {name: digest for package in graph["packages"] for name, digest in package["files"].items()},
        "build_input_hashes": graph["shared_files"],
        "workspace_provenance": {"manifest_sha256": sha(ROOT / "Cargo.toml"), "lock_sha256": sha(ROOT / "Cargo.lock")},
        "reference_archives": {p.name:sha(p) for p in sorted((ROOT/".research/reference/install/lib").glob("*.a"))} if pinned.exists() else {},
        "reference_build": load(pinned) if pinned.exists() else None,
        "reference_source_sha256": sha(ROOT / "harness/reference.c"),
        "lock_sha256": sha(ROOT / "Cargo.lock"),
        "ffprobe_sha256": sha(shutil.which("ffprobe")),
    })
    if system == "Linux":
        identity["host_platform"] = {"system": system, "machine": platform.machine(), "kernel_release": platform.release(),
                                     "kernel_version": platform.version(), "libc": list(platform.libc_ver()),
                                     "page_size": os.sysconf("SC_PAGE_SIZE"), "clock_ticks": os.sysconf("SC_CLK_TCK"),
                                     "nm_version": subprocess.check_output(["nm", "--version"], text=True),
                                     "readelf_version": subprocess.check_output(["readelf", "--version"], text=True),
                                     "ps_version": subprocess.check_output(["ps", "--version"], text=True),
                                     "tool_hashes": {name: sha(shutil.which(name)) for name in ("nm", "readelf", "ps")}}
        identity["linked_libraries"] = {name: linux_library_identity(data) for name, data in libraries.items()}
        identity["linux_loader_observations_path"] = "target/harness/linux-loader-observations.json"
        # Existing collectors bind complete comparison-reference records before
        # reusing first-clean observations. Include candidate/runtime dependencies
        # in each selected reference, not merely the reference executable hash.
        bindings = {name: linux_library_identity(data) for name, data in libraries.items() if name in {"candidate", "reference", "combined_reference"}}
        for record in identity["comparison_references"].values():
            record["linked_library_bindings"] = bindings
    write_json(out / "identity.json", identity)
    if system == "Linux":
        write_json(out / "linux-loader-observations.json", {
            "identity_sha256": sha(out / "identity.json"), "raw_loader_observations": libraries})
    print(out / "identity.json")


if __name__ == "__main__":
    main()
