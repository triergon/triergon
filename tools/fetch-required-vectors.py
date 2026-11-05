#!/usr/bin/env python3
"""Cache hash-pinned required test vectors locally; never vendor their bytes."""
import hashlib
from http.client import HTTPException
import json
from pathlib import Path, PurePosixPath
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
MAXIMUM_BYTES = 8 * 1024 * 1024


def download(relative):
    request = urllib.request.Request(
        'https://fate-suite.ffmpeg.org/' + str(relative),
        headers={'User-Agent': 'triergon-required-test-vectors'})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read(MAXIMUM_BYTES + 1)
    except (OSError, HTTPException) as https_error:
        # FFmpeg also serves the FATE corpus over rsync. The caller checks
        # the same immutable digest for either transport before caching it.
        with tempfile.TemporaryDirectory(prefix='triergon-fate-') as directory:
            temporary = Path(directory) / 'vector'
            try:
                subprocess.run([
                    'rsync', '--contimeout=10', '--timeout=20',
                    f'--max-size={MAXIMUM_BYTES}', '--',
                    'rsync://fate-suite.ffmpeg.org/fate-suite/' + str(relative), str(temporary),
                ], check=True, timeout=35)
                with temporary.open('rb') as vector:
                    return vector.read(MAXIMUM_BYTES + 1)
            except (OSError, subprocess.SubprocessError) as error:
                raise RuntimeError(f'could not fetch required vector {relative} over HTTPS '
                                   f'({https_error}) or rsync') from error


def fetch_required(root=ROOT):
    cache = (root / '.research/fate-samples').resolve()
    contract = json.loads((root / 'harness/catalogue-slice.json').read_text())
    verified = []
    for vector in contract['required_additional_vectors']:
        relative = PurePosixPath(vector['file'])
        if relative.is_absolute() or '..' in relative.parts or '\\' in str(relative):
            raise ValueError('invalid required vector path')
        path = (cache / str(relative)).resolve()
        if not path.is_relative_to(cache):
            raise ValueError('required vector escapes the local cache')
        expected = vector['sha256']
        if path.exists():
            with path.open('rb') as cached:
                data = cached.read(MAXIMUM_BYTES + 1)
            if len(data) > MAXIMUM_BYTES:
                raise ValueError('cached required vector exceeds download bound')
            if hashlib.sha256(data).hexdigest() != expected:
                raise ValueError('cached required vector changed: ' + str(relative))
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            data = download(relative)
            if len(data) > MAXIMUM_BYTES:
                raise ValueError('required vector exceeds download bound')
            if hashlib.sha256(data).hexdigest() != expected:
                raise ValueError('required vector download hash mismatch: ' + str(relative))
            temporary_path = None
            try:
                with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
                    temporary_path = Path(temporary.name)
                    temporary.write(data)
                temporary_path.replace(path)
            finally:
                if temporary_path is not None:
                    temporary_path.unlink(missing_ok=True)
        verified.append(str(relative))
    return verified


if __name__ == '__main__':
    print('Verified required vectors:', ', '.join(fetch_required()))
