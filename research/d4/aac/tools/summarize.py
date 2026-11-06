"""Condense run_all.sh output to one line per vector: pass, worst-channel RMS / max|d|, samples.

usage: research/d4/aac/tools/run_all.sh | python3.14 -I research/d4/aac/tools/summarize.py
"""
import json
import sys

for line in sys.stdin:
    parts = [p.strip() for p in line.split(' | ')]
    if len(parts) < 4:
        continue
    try:
        c = json.loads(parts[3])
    except ValueError:
        print(parts[0], 'unparsed:', parts[3][:120])
        continue
    rms = max(ch['rms'] for ch in c['channels'])
    mx = max(ch['max'] for ch in c['channels'])
    print('%-26s %-4s rms %.3e max %.3e  %d/%d' % (parts[0], 'pass' if c['pass'] else 'FAIL', rms, mx,
                                                   c['cand_samples'], c['ref_samples']))
