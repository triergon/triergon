#!/usr/bin/env python3
"""Check exact core arithmetic against an independent integer/Fraction oracle.

This is sampled correctness evidence, not FFmpeg parity or numerical qualification.
Expected values and Rust observations are produced in separate processes.
"""
import argparse
import hashlib
import itertools
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def graph():
    """The provenance graph of triergon-core plus the oracle sources (cargo xtask graph)."""
    output = subprocess.check_output(
        ['cargo', 'xtask', 'graph', 'triergon-core', 'tools/generate-time-oracle.py',
         'tools/generate-time-edge-oracle.py', 'tools/test-core-time.py', 'xtask/src/harness/provenance.rs'],
        cwd=ROOT, env=dict(os.environ, CARGO_INCREMENTAL='0'), text=True)
    return json.loads(output)


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / '.research/core-time-check')
    parser.add_argument('--profiles', nargs='+', choices=('debug', 'release'), default=['debug', 'release'])
    parser.add_argument('--target', help='the Rust target to build the probe for; the host when omitted')
    parser.add_argument('--toolchain', default='nightly',
                        help='the rustup toolchain to build the probe with; cargo xtask check passes its pinned nightly')
    args = parser.parse_args()
    destination = args.output.resolve()
    out = destination / 'runs' / str(time.time_ns())
    oracle = out / 'oracle'
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_INCREMENTAL='0')
    before = graph()
    for package in before['packages']:
        for name, digest in package['files'].items():
            target = out / 'source' / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
            if sha(target) != digest:
                raise ValueError('source moved while freezing arithmetic input: ' + name)
    for name in before['shared_files']:
        target = out / 'source' / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / name, target)
    subprocess.run([sys.executable, ROOT / 'tools/generate-time-oracle.py', '--output', oracle],
                   cwd=ROOT, env=env, check=True)
    manifest = json.loads((oracle / 'manifest.json').read_text())
    for entry in manifest['files']:
        if sha(oracle / entry['path']) != entry['sha256']:
            raise ValueError('oracle output hash mismatch: ' + entry['path'])
    required = {'time': 34502, 'wide': 15024}
    if manifest['row_counts'] != required:
        raise ValueError('exact arithmetic corpus coverage changed; review the versioned contract')
    edge = ROOT / '.research/core-time-edge-oracle/v2'
    subprocess.run([sys.executable, ROOT / 'tools/generate-time-edge-oracle.py', '--output', edge],
                   cwd=ROOT, env=env, check=True)
    edge_manifest = json.loads((edge / 'manifest.json').read_text())
    edge_required = {'time': 1280, 'wide': 429}
    if edge_manifest.get('corpus_version') != 'core-time-edge-v2' or edge_manifest['row_counts'] != edge_required:
        raise ValueError('supplemental arithmetic corpus version/coverage changed')
    for entry in edge_manifest['files']:
        if sha(edge / entry['path']) != entry['sha256']:
            raise ValueError('supplemental oracle output hash mismatch: ' + entry['path'])
    suites = [(name, oracle, name, count) for name, count in required.items()]
    suites.extend(('edge-' + name, edge, name, count) for name, count in edge_required.items())
    profiles = {}
    for profile in dict.fromkeys(args.profiles):
        command = ['cargo', '+' + args.toolchain, 'build', '-p', 'triergon-core', '--example', 'time-oracle', '--locked']
        if profile == 'release':
            command.append('--release')
        if args.target:
            command.extend(['--target', args.target])
        subprocess.run(command, cwd=ROOT, env=env, check=True)
        built = ROOT / 'target' / args.target if args.target else ROOT / 'target'
        probe = built / profile / 'examples/time-oracle'
        executable = sha(probe)
        shutil.copyfile(probe, out / (profile + '-time-oracle'))
        if sha(out / (profile + '-time-oracle')) != executable:
            raise ValueError('arithmetic probe moved while freezing its executable')
        cases = {}
        for suite, corpus, input_suite, count in suites:
            observed = out / (profile + '-actual-' + suite + '.tsv')
            stderr = out / (profile + '-stderr-' + suite + '.txt')
            with (corpus / ('vectors-' + input_suite + '.tsv')).open('rb') as source, observed.open('wb') as output, stderr.open('wb') as errors:
                run = subprocess.run([probe], stdin=source, stdout=output, stderr=errors, env=env, timeout=60)
            differences = []
            rows = 0
            with (corpus / ('expected-' + input_suite + '.tsv')).open() as expected, observed.open() as actual:
                for index, (left, right) in enumerate(itertools.zip_longest(expected, actual)):
                    rows += 1
                    if left != right:
                        differences.append({'line': index + 1, 'expected': left, 'actual': right})
            passed = run.returncode == 0 and not differences and rows == count
            cases[suite] = {'status': 'pass' if passed else 'mismatch', 'rows': rows,
                            'returncode': run.returncode, 'differences': differences,
                            'actual_sha256': sha(observed), 'stderr_sha256': sha(stderr)}
            print(f'{profile}/{suite}: {rows} cases, {len(differences)} mismatches, exit {run.returncode}', flush=True)
        profiles[profile] = {'cases': cases, 'probe_sha256': executable, 'build_command': command,
                             'probe_unchanged': executable == sha(probe)}
    unchanged = before == graph() and all(profile['probe_unchanged'] for profile in profiles.values())
    passed = unchanged and all(case['status'] == 'pass' for profile in profiles.values() for case in profile['cases'].values())
    report = {'schema': 2, 'status': 'pass' if passed else 'fail',
              'scope': 'sampled exact arithmetic correctness; no FFmpeg/performance/complete-module qualification',
              'selected_graph': before, 'source_and_probe_unchanged': unchanged,
              'profiles': profiles, 'platform': platform.platform(), 'machine': platform.machine(),
              'rustc': subprocess.check_output(['rustc', '+nightly', '-Vv'], text=True, env=env),
              'oracle_manifest_sha256': sha(oracle / 'manifest.json'),
              'edge_manifest_sha256': sha(edge / 'manifest.json'),
              'run_directory': str(out.relative_to(ROOT)) if out.is_relative_to(ROOT) else str(out)}
    (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    # A convenience latest receipt; every real run/source/probe/output above is
    # kept separately, including any failure or interrupted partial output.
    (destination / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print('exact arithmetic ' + report['status'])
    return 0 if report['status'] == 'pass' else 1


if __name__ == '__main__':
    raise SystemExit(main())
