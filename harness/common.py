"""Hashing and JSON helpers shared by the harness and tools (standard library only)."""
import functools
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    """SHA-256 hex digest of a file's bytes."""
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def digest_bytes(data):
    return hashlib.sha256(data).hexdigest()


def canonical_digest(value, **options):
    """SHA-256 of compact key-sorted JSON; options go to json.dumps."""
    return digest_bytes(json.dumps(value, sort_keys=True, separators=(",", ":"), **options).encode())


def load(path):
    return json.loads(Path(path).read_text())


def write_json(path, value, **options):
    """Write JSON (indent=2 unless overridden) with a trailing newline."""
    Path(path).write_text(json.dumps(value, **{"indent": 2, **options}) + "\n")


def save_json(path, value, **options):
    """Atomically replace path with write_json output through a sibling .tmp file."""
    temporary = Path(path).with_suffix(".tmp")
    write_json(temporary, value, **options)
    temporary.replace(path)


def vint(value):
    """Shortest EBML size whose all-ones value stays reserved."""
    width = next(w for w in range(1, 9) if value < (1 << (7 * w)) - 1)
    return ((1 << (7 * width)) | value).to_bytes(width, "big")


def element(identifier, data):
    """One EBML element: identifier as written, shortest size, payload."""
    return identifier.to_bytes((identifier.bit_length() + 7) // 8, "big") + vint(len(data)) + data


def uint(identifier, value):
    return element(identifier, value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big"))


@functools.cache
def _xtask():
    env = dict(os.environ, CARGO_INCREMENTAL="0")
    output = subprocess.check_output(["cargo", "build", "-q", "-p", "xtask", "--message-format=json"], cwd=ROOT, env=env, text=True)
    return next(json.loads(line)["executable"] for line in output.splitlines()
                if '"executable"' in line and json.loads(line).get("executable"))


def xtask_fixture(*args):
    """Bytes of one authored corpus variant (`cargo xtask fixture ...`)."""
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "fixture.mkv"
        subprocess.run([_xtask(), "fixture", *map(str, args), "--output", path], check=True, cwd=ROOT)
        return path.read_bytes()


def zlib_fixture(kind="dynamic", scope=1, lace=0, aac=False, mixed=False, empty=False):
    flags = [f for f, on in (("--aac", aac), ("--mixed", mixed), ("--empty", empty)) if on]
    return xtask_fixture("zlib", "--kind", kind, "--scope", scope, "--lace", lace, *flags)
