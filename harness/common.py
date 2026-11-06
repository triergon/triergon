"""Hashing and JSON helpers shared by the harness and tools (standard library only)."""
import hashlib
import json
from pathlib import Path


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
