"""Per-frame log messages of the reference for coupled-pair SBR probe frames.

Each test frame is followed by a separator frame: an all-zero pair data run whose fill
element is declared 2 bytes long, so the reference reports "Expected to read 2 SBR bytes
actually read N" and then fails the rest of that block. The messages between two
separator reports, without the separator block's own failure messages, are the test
frame's."""
import collections
import re
import subprocess

import numpy as np

import psana
import sbrcpe

SEP_TAIL = '0' * 8


def messages(path, tests, seed=3):
    """tests: list of (ch0, ch1, tail). Returns a list of message lists, one per test."""
    g = sbrcpe.Gen(np.random.default_rng(seed))
    frames = []
    sep = lambda: g.frame('0' * 200, '', SEP_TAIL, fil_bytes=2)
    frames.append(sep())
    for ch0, ch1, tail in tests:
        frames.append(g.frame(ch0, ch1, tail))
        frames.append(sep())
    sbrcpe.stream(path, frames)
    r = subprocess.run([psana.FFMPEG, '-nostdin', '-loglevel', 'repeat+error', '-f', 'aac',
                        '-i', path, '-f', 'null', '-'], capture_output=True, text=True)
    lines = re.findall(r'^\[aac @ (0x[0-9a-f]+)\] (.*)$', r.stderr, re.M)
    main = collections.Counter(a for a, _ in lines).most_common(1)[0][0]
    groups, cur = [], None
    for a, msg in lines:
        if a != main:
            continue
        if msg.startswith('Expected to read 2 SBR bytes'):
            if cur is not None:
                groups.append(cur)
            cur = []
            continue
        if cur is None or msg.startswith('channel element') or 'is not allocated' in msg:
            continue
        cur.append(msg)
    groups.append(cur or [])
    if len(groups) != len(tests) + 1:
        raise RuntimeError('%d groups for %d tests' % (len(groups), len(tests)))
    return groups[:len(tests)]


def ps_read(msgs):
    """The PS bit count a test frame's messages report, or None."""
    for m in msgs:
        x = re.match(r'Expected to read 6 PS bits actually read (\d+)\.', m)
        if x:
            return int(x.group(1))
    return None
