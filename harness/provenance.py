"""Content identity of one worker's selected, owned Cargo dependency graph.

Workspace inventory is separate provenance. An unrelated workspace member is not
part of an existing worker's runtime graph. Shared build settings, package source,
assets/test fixtures, resolved features/edges and explicit harness inputs remain
bound. Exact executable hashes and numerical evidence checks are still required.
"""
import copy
import json
import os
from pathlib import Path
import subprocess
import tomllib

from .common import canonical_digest, digest as file_hash


def cargo_metadata(root):
    env = dict(os.environ, CARGO_INCREMENTAL='0')
    compiler = subprocess.check_output(['rustc', '+nightly', '-Vv'], text=True, env=env)
    target = next(line.removeprefix('host: ') for line in compiler.splitlines() if line.startswith('host: '))
    return json.loads(subprocess.check_output(['cargo', '+nightly', 'metadata', '--format-version', '1',
        '--locked', '--offline', '--filter-platform', target], cwd=root, env=env))


def matroska_inputs(root):
    # Explicit operation inputs avoid making another format's new harness files
    # implicit dependencies of this worker. Adding a Matroska operation updates
    # this list and therefore requires fresh collection identity/source proofs.
    names = ['Makefile', 'specs/sources.json', 'tools/build_harness.py', 'tools/test_media.py',
        'tools/test_catalogue_vector.py', 'tools/run_fate.py', 'research/fate-index.json',
        'harness/common.py', 'harness/provenance.py', 'harness/runner.py', 'harness/memory.py', 'harness/reuse.py', 'harness/comparison.py',
        'harness/spec_oracle.py', 'harness/content_oracle.py', 'harness/colour_oracle.py',
        'harness/catalogue_oracle.py', 'harness/reference.c', 'harness/reference_catalogue.h',
        'harness/reference_colour.h', 'harness/reference_content_encoding.h',
        'harness/allocations.c', 'harness/allocations-linux.c', 'harness/memory-check.c',
        'harness/portable-memory-check.c', 'harness/rss-bound-api.h', 'harness/comparison-rules.json',
        'harness/fate-map.json']
    for prefix in ('', 'header-', 'zlib-', 'seek-', 'colour-', 'catalogue-'):
        names.extend('harness/' + prefix + suffix + '.json' for suffix in ('slice', 'corpus'))
    return names


def source_identity_error(root, identity):
    """One fail-closed source check for every Matroska/FATE collector."""
    root = Path(root).resolve()
    try:
        if identity.get('schema') == 2:
            current = selected_graph(root, cargo_metadata(root), 'media-worker', matroska_inputs(root))
            return None if current == identity.get('selected_graph') else 'selected source/configuration changed since build identity'
        if identity.get('schema') == 1:
            # Historical collections keep the exact original whole-workspace rule.
            sources = {str(path.relative_to(root)): file_hash(path) for path in sorted(root.glob('crates/**/src/*.rs'))}
            valid = sources == identity['source_hashes'] and all(file_hash(root / name) == value
                for name, value in identity['build_input_hashes'].items())
            return None if valid else 'source/configuration changed since build identity'
        return 'unsupported build identity schema'
    except (KeyError, ValueError, OSError, subprocess.SubprocessError) as error:
        return 'cannot verify source identity: ' + str(error)


def _file(root, path):
    path = Path(path)
    if not path.is_absolute():
        path = root / path
    if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root):
        raise ValueError('selected input is not a regular repository file: ' + str(path))
    # Reject symlinked parent directories as well as symlinked leaf files.
    lexical_root = next((parent for parent in reversed(path.parents) if parent.resolve() == root), None)
    if lexical_root is None:
        raise ValueError('selected input has no repository ancestor: ' + str(path))
    relative = path.relative_to(lexical_root)
    if '..' in relative.parts:
        raise ValueError('selected input uses parent traversal: ' + str(relative))
    current = root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError('selected input traverses a symlink: ' + str(relative))
    return relative.as_posix(), file_hash(path)


def selected_graph(root, metadata, package_name, extra_files=()):
    """Use Cargo's target-filtered resolved graph, not lexical dependency guesses.

    The caller runs metadata with the same feature/target selection as the build.
    This function admits no registry/git/outside-repository packages or build
    scripts. Build environment, compiler and exact commands are recorded by the
    build controller alongside this graph, rather than inferred here.
    """
    root = Path(root).resolve()
    packages = {package['id']: package for package in metadata['packages']}
    nodes = {node['id']: node for node in (metadata.get('resolve') or {}).get('nodes', [])}
    roots = [key for key, value in packages.items() if value['name'] == package_name]
    if len(roots) != 1:
        raise ValueError('selected package must resolve exactly once: ' + package_name)
    pending, selected = [roots[0]], set()
    while pending:
        key = pending.pop()
        if key in selected:
            continue
        if key not in nodes or key not in packages:
            raise ValueError('selected dependency is missing from Cargo metadata')
        selected.add(key)
        pending.extend(dependency['pkg'] for dependency in nodes[key]['deps'])
    normalized = []
    for key in sorted(selected, key=lambda key: packages[key]['name']):
        package = packages[key]
        if package.get('source') is not None:
            raise ValueError('unadmitted non-owned dependency: ' + package['name'])
        manifest = Path(package['manifest_path'])
        manifest_name, _ = _file(root, manifest)
        directory = manifest.parent
        if directory == root:
            raise ValueError('selected production packages must have their own directory')
        for target in package.get('targets', []):
            if 'custom-build' in target['kind'] or 'proc-macro' in target['kind']:
                raise ValueError('selected build-time code needs a separate admission audit')
            _file(root, target['src_path'])
        sources = {}
        for path in sorted(directory.rglob('*')):
            relative = path.relative_to(directory)
            if any(part in ('target', '.git', '__pycache__', '.DS_Store') for part in relative.parts):
                continue
            if path.is_symlink():
                raise ValueError('symlink in selected package: ' + str(relative))
            if path.is_file():
                name, digest = _file(root, path)
                sources[name] = digest
        dependencies = []
        for dependency in nodes[key]['deps']:
            other = packages[dependency['pkg']]
            other_manifest, _ = _file(root, other['manifest_path'])
            dependencies.append({'alias': dependency['name'], 'package': other['name'],
                                 'manifest': other_manifest, 'kinds': dependency['dep_kinds']})
        normalized.append({'name': package['name'], 'version': package['version'],
                           'manifest': manifest_name, 'features': sorted(nodes[key]['features']),
                           'dependencies': sorted(dependencies, key=lambda value: (value['alias'], value['manifest'])),
                           'files': sources})
    settings = copy.deepcopy(tomllib.loads((root / 'Cargo.toml').read_text()))
    for key in ('members', 'default-members', 'exclude'):
        settings.get('workspace', {}).pop(key, None)
    shared = {}
    for name in ('.cargo/config', '.cargo/config.toml', 'rust-toolchain', 'rust-toolchain.toml'):
        path = root / name
        if path.exists() or path.is_symlink():
            name, digest = _file(root, path)
            shared[name] = digest
    for path in extra_files:
        name, digest = _file(root, path)
        shared[name] = digest
    graph = {'schema': 1, 'root_package': package_name, 'workspace_settings': settings,
             'packages': normalized, 'shared_files': dict(sorted(shared.items()))}
    return {**graph, 'sha256': canonical_digest(graph)}
