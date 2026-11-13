"""Write crates/codec-aac/src/huffman_probed.rs from the recovered book JSONs (interim; the
xtask generator is to take this over).

usage: gen_books.py OUT.rs [PROBE_OUT_DIR]
"""
import json
import os
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..', '..'))
O = sys.argv[2] if len(sys.argv) > 2 else os.path.join(ROOT, '.research', 'clean-room', 'aac-ps', 'probes', 'out')
books = []
for name, f, off in [('IID_DF', 'huff/iid_df.json', 14), ('IID_DT', 'huff/iid_dt.json', 14),
                     ('IID_DF_FINE', 'huff/iid_df_fine.json', 30), ('IID_DT_FINE', 'huff/iid_dt_fine.json', 30),
                     ('ICC_DF', 'huff/icc_df.json', 7), ('ICC_DT', 'huff/icc_dt.json', 7)]:
    j = json.load(open(os.path.join(O, f)))
    books.append((name, {v + off: c for c, v in j['codewords'].items()}))
j = json.load(open(os.path.join(O, 'ipdopd', 'ipdopd.json')))
for b in ('ipd_df', 'ipd_dt', 'opd_df', 'opd_dt'):
    books.append((b.upper(), {v: c for c, v in j['books'][b].items()}))
j = json.load(open(os.path.join(O, 'envbal', 'sbr_f_envbal30.json')))
books.append(('SBR_F_ENVBAL30', {int(v) + 12: c for c, v in j['codewords'].items()}))
out = ['//! Huffman books recovered from the reference decoder\'s behaviour by the probes in',
       '//! `harness/codecs/aac/ps` (generated; do not edit).', '']
for name, m in books:
    n = len(m)
    assert sorted(m) == list(range(n)), name
    out.append('pub(crate) static %s: [(u32, u8); %d] = [' % (name, n))
    for s in range(n):
        c = m[s]
        out.append('    (0x%x, %d),' % (int(c, 2), len(c)))
    out.append('];')
    out.append('')
open(sys.argv[1], 'w').write('\n'.join(out))
