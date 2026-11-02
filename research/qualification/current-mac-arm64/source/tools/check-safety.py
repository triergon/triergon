#!/usr/bin/env python3
"""Audit the current dependency-free production workspace. Research is excluded."""
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    metadata = json.loads(subprocess.check_output(["cargo", "+nightly", "metadata", "--format-version", "1", "--locked", "--offline"], cwd=ROOT, env=env))
    for package in metadata["packages"]:
        if package["source"] is not None or not Path(package["manifest_path"]).is_relative_to(ROOT / "crates"):
            raise SystemExit("Unadmitted dependency: " + package["name"])
        for target in package["targets"]:
            source = Path(target["src_path"])
            if "custom-build" in target["kind"]:
                raise SystemExit("Build scripts need explicit audit")
            if source.name != "main.rs" and source.name != "lib.rs":
                continue
            if "#![forbid(unsafe_code)]" not in source.read_text():
                raise SystemExit("Missing crate-level unsafe prohibition: " + str(source))
    for source in (ROOT / "crates").rglob("*.rs"):
        if re.search(r"\bunsafe\s*(?:\{|impl|trait|fn|extern)", source.read_text()):
            raise SystemExit("Unsafe syntax requires investigation: " + str(source))
    print(f"Strict-safe workspace: {len(metadata['packages'])} owned packages; no third-party packages, build scripts or unsafe syntax")


if __name__ == "__main__":
    main()
