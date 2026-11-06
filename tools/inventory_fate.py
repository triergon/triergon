#!/usr/bin/env python3
"""Index FATE definitions as data; never execute Make or downloaded shell code.

This is a lexical inventory. Case-local SRC and literal sample references are
recorded, but Make functions, generated targets and conditionals are not evaluated.
Unresolved paths cannot become acquired inputs or executed-test claims.
"""
import argparse
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from harness.common import digest_bytes, write_json


CASE = r'fate-[a-zA-Z0-9][a-zA-Z0-9_.-]*'
ASSIGNMENT = re.compile(r'^(' + CASE + r')\s*:\s*(CMD|SRC)\s*=\s*(.*)$')
SAMPLE = re.compile(r'\$\(TARGET_SAMPLES\)/([^\s"\']+)')


def inventory(source):
    cases, definitions = {}, {}
    for path in sorted((source / 'tests/fate').glob('*.mak')):
        data = path.read_bytes()
        relative = str(path.relative_to(source))
        definitions[relative] = digest_bytes(data)
        text = data.decode()
        for number, line in enumerate(text.splitlines(), 1):
            if line.lstrip().startswith('#'):
                continue
            for name in re.findall(r'\b' + CASE, line):
                row = cases.setdefault(name, dict(id=name, definition_files=[],
                    command_sha256=[], sample_references=[], unresolved_sample_references=[],
                    status='unmapped', rights='unreviewed', assignments=[]))
                location = f'{relative}:{number}'
                if location not in row['definition_files']:
                    row['definition_files'].append(location)
        for line in text.replace('\\\n', ' ').splitlines():
            match = ASSIGNMENT.match(line)
            if match and match[1] in cases:
                cases[match[1]]['assignments'].append((match[2], match[3]))
    for row in cases.values():
        assignments = row.pop('assignments')
        sources = [value for key, value in assignments if key == 'SRC']
        for key, value in assignments:
            if key == 'CMD':
                row['command_sha256'].append(digest_bytes(value.encode()))
                texts = [value.replace('$(SRC)', source) for source in sources] or [value]
            else:
                texts = [value]
            for text in texts:
                for sample in SAMPLE.findall(text):
                    field = 'unresolved_sample_references' if '$' in sample else 'sample_references'
                    if sample not in row[field]:
                        row[field].append(sample)
    return {'schema_version': 1,
            'status': 'identity inventory only; not the configure-expanded runnable FATE suite',
            'source_root_role': 'externally cached FFmpeg snapshot',
            'definition_sha256': definitions,
            'cases': sorted(cases.values(), key=lambda row: row['id'])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--configured-list', type=Path,
                        help='annotate configured identities; this does not execute a case')
    args = parser.parse_args()
    report = inventory(args.source)
    if args.configured_list:
        data = args.configured_list.read_bytes()
        enabled = set(data.decode().split())
        report['configured_list'] = {'sha256': digest_bytes(data), 'identities': len(enabled),
                                     'executed': False}
        for row in report['cases']:
            row['configured'] = row['id'] in enabled
    args.output.parent.mkdir(parents=True, exist_ok=True)
    write_json(args.output, report)
    print(f"Indexed {len(report['cases'])} identities; none executed or accepted.")


if __name__ == '__main__':
    main()
