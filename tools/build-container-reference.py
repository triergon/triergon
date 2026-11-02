#!/usr/bin/env python3
"""Build a pinned public-API observation worker, not a qualification receipt."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--format', choices=('mpegts', 'mov'), required=True)
    args = parser.parse_args()
    cache = ROOT / '.research/reference-formats'
    build = json.loads((cache / 'build.json').read_text())
    if build.get('profile') != 'formats' or build.get('revision') != 'd32b387f2b0a484599d4587d651891f0c63c4238':
        raise ValueError('isolated formats reference is not the selected source/configuration')
    for name, digest in build['reference_archives'].items():
        if sha(cache / name) != digest:
            raise ValueError('reference archive changed: ' + name)
    env = dict(os.environ, PKG_CONFIG_LIBDIR=str(cache / 'install/lib/pkgconfig'))
    flags = shlex.split(subprocess.check_output(['pkg-config', '--static', '--cflags', '--libs',
                       'libavformat', 'libavcodec', 'libavutil'], env=env, text=True))
    out = ROOT / 'target/harness'
    out.mkdir(parents=True, exist_ok=True)
    binary = out / (args.format + '-ffmpeg-worker')
    source = ROOT / 'harness/container_reference.c'
    command = [os.environ.get('CC', 'cc'), '-O3', '-Wall', '-Wextra', '-Werror']
    if args.format == 'mov':
        command.append('-DSAFE_MEDIA_REFERENCE_MOV=1')
    command.extend([str(source), *flags, '-o', str(binary)])
    before = sha(source)
    subprocess.run(command, check=True)
    if before != sha(source):
        raise ValueError('reference source changed during build')
    record = {'schema': 1, 'scope': 'actual native packet observations; no candidate parity or numerical qualification',
              'format': args.format, 'source_sha256': before, 'worker_sha256': sha(binary),
              'command': command, 'reference_build': build,
              'compiler': subprocess.check_output([command[0], '--version'], text=True),
              'identity': json.loads(subprocess.check_output([binary, 'identity'], text=True))}
    identity = out / (args.format + '-reference-identity.json')
    identity.write_text(json.dumps(record, indent=2) + '\n')
    print(identity)


if __name__ == '__main__':
    main()
