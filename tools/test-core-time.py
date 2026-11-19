#!/usr/bin/env python3
"""Compatibility entry point for cargo xtask core-time."""
import os
from pathlib import Path
import sys


if __name__ == "__main__":
    manifest = Path(__file__).resolve().parents[1] / "xtask/Cargo.toml"
    os.environ["CARGO_INCREMENTAL"] = "0"
    os.environ.setdefault("TRIERGON_PYTHON", sys.executable)
    os.execvp("cargo", ["cargo", "run", "--quiet", "--manifest-path", str(manifest),
                        "--", "core-time", *sys.argv[1:]])
