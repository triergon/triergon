#!/usr/bin/env python3
"""Verify the O00 source receipt against an explicitly selected Streem checkout.

Read-only source integrity, not media/binary/platform qualification. A changed
revision requires a new scan for additional uses rather than just old hash checks.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--source', type=Path, required=True)
    args = parser.parse_args()
    source = args.source.resolve()
    receipt = json.loads((ROOT / 'research/offboarding/streem-source-inventory/source-files.json').read_text())
    def git(*arguments):
        return subprocess.check_output(['git', '-C', source, *arguments], text=True).strip()
    before = git('rev-parse', 'HEAD')
    if before != receipt['source_revision'] or git('status', '--porcelain'):
        raise SystemExit('Source revision/worktree differs; refresh the inventory before acceptance')
    names = []
    for row in receipt['files']:
        relative = Path(row['path'])
        path = source / relative
        if relative.is_absolute() or '..' in relative.parts or not path.resolve().is_relative_to(source):
            raise SystemExit('Invalid inventory path')
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != row['sha256'] or len(data) != row['bytes']:
            raise SystemExit('Source changed: ' + relative.as_posix())
        names.append(relative.as_posix())
    if len(names) != receipt['file_count'] or len(set(names)) != len(names):
        raise SystemExit('Duplicate or incomplete source accounting')
    subprocess.run(['git', '-C', source, 'ls-files', '--error-unmatch', '--', *names],
                   stdout=subprocess.DEVNULL, check=True)
    if git('rev-parse', 'HEAD') != before or git('status', '--porcelain'):
        raise SystemExit('Source changed during review')
    print(f'O00 source receipt: {len(names)} files match {before}; media/binary/platform qualification remains open')


if __name__ == '__main__':
    main()
