#!/usr/bin/env python3
"""Which checks a change needs: `docs` or `full`.

  ci-scope.py BASE HEAD   print `docs` when every file changed from BASE to HEAD
                          is one only `cargo xtask check docs` reads, else `full`

A changed file is a documentation change when it is a regular file and

  - lies under docs/, which no test or build reads, or
  - lies under harness/evidence/sets/, which only the evidence index tests read, or
  - is a Markdown file edited in place (not added, removed or renamed), outside
    harness/ and any tests/ or fixtures/ directory, that no Rust source in HEAD
    includes (`include_str!`, `include_bytes!`) by name, and whose file name no
    source with an include built from parts (`concat!`, a macro argument) mentions.

Anything else, an empty change, or a change this script cannot read is `full`.
"""
import os
import re
import subprocess
import sys

DOCUMENTS = 'docs/'
EVIDENCE_SETS = 'harness/evidence/sets/'
REGULAR = {'100644', '100755'}
ABSENT = '000000'
MACRO = re.compile(rb'include_(?:str|bytes)!\s*\(')
STRING = re.compile(rb'"((?:[^"\\]|\\.)*)"')


def git(*args):
    return subprocess.check_output(['git', *args])


def changes(base, head):
    """(status, old mode, new mode, path) of every file changed from BASE to HEAD."""
    raw = git('diff', '--raw', '--no-renames', '--no-abbrev', '-z', base, head)
    fields = raw.split(b'\0')
    out = []
    for header, path in zip(fields[0::2], fields[1::2]):
        old_mode, new_mode, _, _, status = header.decode().lstrip(':').split(' ')
        out.append((status, old_mode, new_mode, os.fsdecode(path)))
    return out


def arguments(text):
    """The argument text of every `include_str!(...)` and `include_bytes!(...)` in
    TEXT, by matching parentheses (so an argument may span lines)."""
    for macro in MACRO.finditer(text):
        depth, at = 1, macro.end()
        while depth and at < len(text):
            depth += {ord('('): 1, ord(')'): -1}.get(text[at], 0)
            at += 1
        if depth:
            raise ValueError('unbalanced include')
        yield text[macro.end():at - 1]


class Includes:
    """What the Rust sources in a commit include: every path named by a single
    string literal, and the text of each source with an include built from parts
    (`concat!`, a macro argument) that may name a Markdown file."""

    def __init__(self, head):
        listed = subprocess.run(
            ['git', 'grep', '-l', '-z', '-I', '-E', r'include_(str|bytes)!', head, '--', '*.rs'],
            stdout=subprocess.PIPE, check=False)
        if listed.returncode not in (0, 1):
            raise RuntimeError('git grep failed')
        self.paths = set()
        self.unresolved = []
        for name in filter(None, listed.stdout.split(b'\0')):
            source = os.fsdecode(name.split(b':', 1)[1])
            text = git('show', name.decode())
            for argument in arguments(text):
                literals = STRING.findall(argument)
                if len(literals) == 1 and STRING.fullmatch(argument.strip()):
                    target = os.path.join(os.path.dirname(source), os.fsdecode(literals[0]))
                    self.paths.add(os.path.normpath(target))
                elif not names_other_file(argument, literals):
                    self.unresolved.append(text)
                    break

    def may_include(self, path):
        name = os.path.basename(path).encode()
        return path in self.paths or any(name in text for text in self.unresolved)


def names_other_file(argument, literals):
    """Whether an include built from parts ends in a literal with an extension other
    than `.md`, as `concat!(env!("CARGO_MANIFEST_DIR"), "/tests/a.ts")` does."""
    if not literals:
        return False
    last = literals[-1]
    tail = argument[argument.rindex(b'"' + last + b'"') + len(last) + 2:]
    extension = os.path.splitext(os.fsdecode(last))[1]
    return extension not in ('', '.md') and not tail.strip(b' \t\r\n,)')


def documentation(change, includes):
    status, old_mode, new_mode, path = change
    modes = {old_mode, new_mode} - {ABSENT}
    if not modes or not modes <= REGULAR:
        return False
    if path.startswith(DOCUMENTS) or path.startswith(EVIDENCE_SETS):
        return True
    parts = path.split('/')
    return (
        status == 'M'
        and path.endswith('.md')
        and parts[0] != 'harness'
        and not {'tests', 'fixtures'} & set(parts[:-1])
        and not includes.may_include(path)
    )


def scope(base, head):
    changed = changes(base, head)
    if not changed:
        return 'full'
    includes = Includes(head)
    return 'docs' if all(documentation(change, includes) for change in changed) else 'full'


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    try:
        print(scope(sys.argv[1], sys.argv[2]))
    except (subprocess.CalledProcessError, RuntimeError, ValueError, UnicodeDecodeError) as error:
        print(f'cannot classify the change ({error}); running every check', file=sys.stderr)
        print('full')


if __name__ == '__main__':
    main()
