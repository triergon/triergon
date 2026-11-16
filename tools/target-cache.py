#!/usr/bin/env python3
"""Keep a cached cargo target directory fresh by content, not by checkout time.

Cargo decides whether a build is fresh by comparing the modification times of its
sources with those of its output. A fresh checkout gives every file the time of the
checkout, so a restored target directory would be rebuilt whole. This script lets
cargo reuse exactly the output whose sources are unchanged:

  record TARGET   before the directory is cached: write TARGET/.sources, the git
                  object of every tracked file (`git ls-files -s`). Refused, with
                  nothing written, when a tracked file differs from the index, since
                  the record would then not describe what was built.
  apply TARGET    after the directory is restored: every tracked file whose path,
                  mode and object match the record gets a fixed time in 2000, older
                  than any output; every other file gets the current time, newer
                  than any output, so cargo rebuilds whatever reads it. A restored
                  directory without a record is removed.

Files outside git (generated fixtures, downloads) keep the times they were written
with in this job, which are newer than the restored output.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

RECORD = '.sources'
# 2000-01-01T00:00:00Z: older than any cached output.
UNCHANGED = 946684800


def tracked():
    """The `git ls-files -s` entries: (mode, object, path) of every tracked file."""
    listing = subprocess.check_output(['git', 'ls-files', '-s', '-z'])
    return [entry for entry in listing.split(b'\0') if entry]


def entry_path(entry):
    return os.fsdecode(entry.split(b'\t', 1)[1])


def record(target):
    changed = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=no'])
    if changed:
        sys.exit('tracked files differ from the index; not recording the target cache:\n'
                 + changed.decode(errors='replace'))
    target.mkdir(parents=True, exist_ok=True)
    (target / RECORD).write_bytes(b'\0'.join(tracked()))


def apply(target):
    source = target / RECORD
    if not source.is_file():
        if target.exists():
            print(f'{target}: restored without a source record; removing it')
            shutil.rmtree(target)
        return
    recorded = set(source.read_bytes().split(b'\0'))
    source.unlink()
    unchanged = changed = 0
    for entry in tracked():
        path = entry_path(entry)
        if not os.path.lexists(path):
            continue
        if entry in recorded:
            os.utime(path, (UNCHANGED, UNCHANGED), follow_symlinks=False)
            unchanged += 1
        else:
            os.utime(path, None, follow_symlinks=False)
            changed += 1
    print(f'{target}: {unchanged} tracked files unchanged since the cached build, {changed} changed')


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('command', choices=('record', 'apply'))
    parser.add_argument('target', type=Path)
    args = parser.parse_args()
    {'record': record, 'apply': apply}[args.command](args.target)


if __name__ == '__main__':
    main()
