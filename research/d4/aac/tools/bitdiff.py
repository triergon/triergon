"""Max |a - b| between two f32le files of equal length (dev aid for refactor checks).

usage: python3.14 -I bitdiff.py DIR_A DIR_B   (compares every *.f32 present in both)
"""
import os
import sys
import numpy as np

a, b = sys.argv[1], sys.argv[2]
for n in sorted(os.listdir(a)):
    if not n.endswith('.f32') or not os.path.exists(os.path.join(b, n)):
        continue
    x = np.fromfile(os.path.join(a, n), dtype='<f4')
    y = np.fromfile(os.path.join(b, n), dtype='<f4')
    if len(x) != len(y):
        print('%-30s length %d vs %d' % (n, len(x), len(y)))
        continue
    d = np.abs(x.astype(np.float64) - y)
    print('%-30s %s max %.3e' % (n, 'identical' if not d.any() else 'differs', d.max() if len(d) else 0))
