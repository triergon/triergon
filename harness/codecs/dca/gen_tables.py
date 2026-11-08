#!/usr/bin/env python3
"""Generate crates/codec-dca/src/tables.rs from the text of ETSI TS 102 114 V1.6.1.

Input: `pdftotext -layout ts_102114v010601p.pdf spec.txt` (PDF sha256
29f9e50575e48bfe55222c5f004088af379bc9a6c65274ed975a81dca10647ba).
Every table is transcribed mechanically from Annex D / clause 6 of that text and
validated (entry counts, level ranges, Kraft equality and prefix freedom for the
Huffman books) before any Rust is emitted.
Usage: python3 -I gen_tables.py SPEC_V161_TXT OUT_RS SPEC_V131_TXT
(V1.3.1: https://www.etsi.org/deliver/etsi_ts/102100_102199/102114/01.03.01_60/ts_102114v010301p.pdf)
"""
import re, sys

spec_path, out_path = sys.argv[1], sys.argv[2]
lines = open(spec_path, encoding='utf-8').read().split('\n')

def find(pat, start=0):
    for i in range(start, len(lines)):
        if re.match(pat, lines[i]) and '.....' not in lines[i]:
            return i
    raise SystemExit('pattern not found: ' + pat)

def is_page_noise(l):
    s = l.strip()
    return s == 'ETSI' or 'ETSI TS 102 114' in s

NUM = re.compile(r'-?\d{1,3}(?: \d{3})+(?!\d)|-?\d+')

# ---------------------------------------------------------------- Huffman (D.5)
def parse_books(lines):
    def find_in(pat, start=0):
        for i in range(start, len(lines)):
            if re.match(pat, lines[i]) and '.....' not in lines[i]:
                return i
        raise SystemExit('pattern not found: ' + pat)
    d5 = find_in(r'^D\.5\s+Huffman Code Books')
    d6 = find_in(r'^D\.6\s+Block Code Books', d5)
    books = {}
    order = []
    heads = []  # list of (name, column)
    prev_was_head = False
    for i in range(d5, d6):
        l = lines[i]
        if is_page_noise(l):
            continue
        hs = [(m.group(1), m.start()) for m in re.finditer(r'Table\s+([A-G]?S?[A-G]\d+)', l)]
        if hs:
            # A heading line that directly follows another heading line extends the group.
            heads = heads + hs if (heads and prev_was_head) else hs
            for n, _ in hs:
                if n not in books:
                    books[n] = []
                    order.append(n)
            prev_was_head = True
            continue
        s = l.strip()
        if re.match(r'^D\.5\.\d+', s):
            heads = []
            prev_was_head = False
            continue
        if not s or not re.fullmatch(r'[-\d ]+', s):
            if s:
                prev_was_head = False
            continue
        prev_was_head = False
        toks = [(m.group(0), m.start()) for m in NUM.finditer(l)]
        if len(toks) % 3 != 0:
            raise SystemExit('odd row at %d: %r' % (i + 1, l))
        triples = [(int(toks[k][0].replace(' ', '')), int(toks[k + 1][0].replace(' ', '')),
                    int(toks[k + 2][0].replace(' ', '')), toks[k][1]) for k in range(0, len(toks), 3)]
        if not heads:
            raise SystemExit('data without heading at %d' % (i + 1))
        if len(heads) == 1:
            for t in triples:
                books[heads[0][0]].append(t[:3])
        elif len(triples) == len(heads):
            for (n, _), t in zip(heads, triples):
                books[n].append(t[:3])
        else:
            for t in triples:
                n = min(heads, key=lambda h: abs(h[1] - t[3]))[0]
                books[n].append(t[:3])
    return books, order

books, order = parse_books(lines)
# Errata handled explicitly (documented in harness/codecs/dca/STATUS.md):
# 1. V1.6.1 D.5.12 Table D129 lists level "37" twice (codes 1582 and 1581, length 11) and
#    omits -37; every other row pair is (+k, code c), (-k, code c-1), so code 1581 is -37.
fix = [k for k, e in enumerate(books['D129']) if e == (37, 11, 1581)]
assert len(fix) == 1 and sum(1 for e in books['D129'] if e[0] == 37) == 2
books['D129'][fix[0]] = (-37, 11, 1581)
# 2. V1.6.1 (and V1.4.1) D.5.11 Table D65 drop levels +-31/+-32 across a page break (Kraft sum
#    0.99988). V1.3.1 (2011-08) prints the complete table; take it from there after checking
#    that every row V1.6.1 does print is identical.
if len(sys.argv) > 3:
    alt_books, _ = parse_books(open(sys.argv[3], encoding='utf-8').read().split('\n'))
    alt = alt_books['D65']
    assert set(books['D65']) <= set(alt), 'D65 mismatch between spec versions'
    for n in order:
        if n != 'D65' and n in alt_books and n != 'D129':
            assert sorted(alt_books[n]) == sorted(books[n]), 'version mismatch in ' + n
    books['D65'] = alt
else:
    raise SystemExit('need V1.3.1 text as third argument for Table D65')

def expected_levels(name):
    n = int(re.sub(r'^[A-Z]+', '', name))
    if name.startswith('S'):
        return list(range(-64, 65))
    if n == 4:
        return list(range(0, 4))
    if n == 12:
        return list(range(1, 13))
    h = (n - 1) // 2
    return list(range(-h, h + 1))

for name in order:
    ent = books[name]
    lv = sorted(e[0] for e in ent)
    if lv != expected_levels(name):
        raise SystemExit('level set mismatch in %s: %d entries' % (name, len(ent)))
    kraft = sum(2.0 ** -e[1] for e in ent)
    if abs(kraft - 1.0) > 1e-12:
        raise SystemExit('Kraft sum %r in %s' % (kraft, name))
    codes = sorted((format(e[2], '0%db' % e[1]) for e in ent))
    for a, b in zip(codes, codes[1:]):
        if b.startswith(a):
            raise SystemExit('prefix clash in %s' % name)
    for e in ent:
        if e[2] >= (1 << e[1]):
            raise SystemExit('code too long in %s' % name)

# Two-level lookup tables. Entry = (symbol: i16, len: i8). len > 0 leaf (total code length);
# len < 0 pointer: symbol = absolute offset of the subtable, -len = subtable index bits.
PRIMARY = 8
def build_lut(ent):
    maxlen = max(e[1] for e in ent)
    pb = min(PRIMARY, maxlen)
    table = [None] * (1 << pb)
    long_groups = {}
    for (sym, ln, code) in ent:
        if ln <= pb:
            base = code << (pb - ln)
            for k in range(1 << (pb - ln)):
                table[base + k] = (sym, ln)
        else:
            prefix = code >> (ln - pb)
            long_groups.setdefault(prefix, []).append((sym, ln, code))
    for prefix in sorted(long_groups):
        grp = long_groups[prefix]
        sb = max(e[1] for e in grp) - pb
        off = len(table)
        table[prefix] = (off, -sb)
        sub = [None] * (1 << sb)
        for (sym, ln, code) in grp:
            rest = code & ((1 << (ln - pb)) - 1)
            r = ln - pb
            base = rest << (sb - r)
            for k in range(1 << (sb - r)):
                sub[base + k] = (sym, ln)
        table.extend(sub)
    if any(t is None for t in table):
        raise SystemExit('incomplete lut')
    return pb, maxlen, table

out = []
w = out.append
w('// @generated by harness/codecs/dca/gen_tables.py from ETSI TS 102 114 V1.6.1 (2019-08) text.')
w('// Do not edit by hand. Each table names its source clause.')
w('#![allow(clippy::unreadable_literal, clippy::excessive_precision)]')
w('')
w('/// Two-level Huffman lookup: `(symbol, len)`; `len > 0` leaf with total code length,')
w('/// `len < 0` subtable at offset `symbol` indexed by the next `-len` bits.')
w('#[derive(Debug)]')
w('pub struct HuffBook { pub primary_bits: u32, pub max_len: u32, pub lut: &\'static [(i16, i8)] }')
w('')
for name in order:
    pb, ml, table = build_lut(books[name])
    w('/// ETSI TS 102 114 Annex D.5 Table %s.' % name)
    w('pub static HUFF_%s: HuffBook = HuffBook { primary_bits: %d, max_len: %d, lut: &[' % (name, pb, ml))
    row = []
    for (s, l) in table:
        row.append('(%d,%d)' % (s, l))
        if len(row) == 16:
            w('    ' + ','.join(row) + ',')
            row = []
    if row:
        w('    ' + ','.join(row) + ',')
    w(']};')
w('')

# ---------------------------------------------------------------- D.1 scale factors
def parse_pairs(start, end, ncols_per_entry, value_cols):
    vals = {}
    for i in range(start, end):
        l = lines[i]
        if is_page_noise(l):
            continue
        s = l.replace('invalid', 'X')
        toks = re.split(r'\s{2,}', s.strip())
        yield i, toks

d11 = find(r'^D\.1\.1')
d12 = find(r'^D\.1\.2', d11)
d2 = find(r'^D\.2\s+Quantization Step Size', d12)
def scale_table(a, b, size):
    vals = {}
    for i in range(a, b):
        l = lines[i]
        if is_page_noise(l):
            continue
        toks = re.split(r'\s{2,}', l.strip())
        if len(toks) == 6 and re.fullmatch(r'\d+', toks[0]):
            for k in (0, 3):
                idx = int(toks[k])
                if toks[k + 1] == 'invalid':
                    continue
                vals[idx] = int(toks[k + 1])
    return vals
s6 = scale_table(d11, d12, 64)
s7 = scale_table(d12, d2, 128)
assert sorted(s6) == list(range(63)), sorted(s6)
assert sorted(s7) == list(range(125)), sorted(s7)
w('/// ETSI TS 102 114 Annex D.1.1: 6-bit scale factor quantization (index 63 invalid -> 0).')
w('pub static SCALE_6BIT: [u32; 64] = [%s, 0];' % ', '.join(str(s6[i]) for i in range(63)))
w('/// ETSI TS 102 114 Annex D.1.2: 7-bit scale factor quantization (indices 125..127 invalid -> 0).')
w('pub static SCALE_7BIT: [u32; 128] = [%s, 0, 0, 0];' % ', '.join(str(s7[i]) for i in range(125)))

d21 = find(r'^D\.2\.1', d2)
d22 = find(r'^D\.2\.2', d21)
d3 = find(r'^D\.3\s+Scale Factor for Joint', d22)
def step_table(a, b):
    vals = {}
    for i in range(a, b):
        toks = re.split(r'\s{2,}', lines[i].strip())
        if len(toks) == 3 and re.fullmatch(r'\d+', toks[0]) and re.fullmatch(r'\d+', toks[1]):
            vals[int(toks[0])] = int(toks[1])
    assert sorted(vals) == list(range(27)), sorted(vals)
    return [vals[i] for i in range(27)]
w('/// ETSI TS 102 114 Annex D.2.1: lossy quantization step size x 2^22, by ABITS.')
w('pub static STEP_LOSSY: [u32; 27] = [%s];' % ', '.join(map(str, step_table(d21, d22))))
w('/// ETSI TS 102 114 Annex D.2.2: lossless quantization step size x 2^22, by ABITS.')
w('pub static STEP_LOSSLESS: [u32; 27] = [%s];' % ', '.join(map(str, step_table(d22, d3))))

d4 = find(r'^D\.4\s+Dynamic Range Control', d3)
js = {}
for i in range(d3, d4):
    l = lines[i]
    toks = re.findall(r'(\d+)\s+(\d+(?:,\d+)?)', l)
    for a, b in toks:
        js[int(a)] = b.replace(',', '.')
assert sorted(js) == list(range(129)), sorted(js)
w('/// ETSI TS 102 114 Annex D.3: joint intensity scale factors (index = code + 64).')
w('pub static JOINT_SCALE: [f32; 129] = [%s];' % ', '.join(js[i] + ('' if '.' in js[i] else '.0') for i in range(129)))

# ---------------------------------------------------------------- D.8 / D.9 FIRs
FLT = r'-?\d,\d+e[-+]\d+'
d8 = find(r'^D\.8\s+32-Band Interpolation')
d9 = find(r'^D\.9\s+1 024 tap FIR', d8)
d10 = find(r'^D\.10\s+VQ Tables', d9)
cols = [dict(), dict(), dict(), dict()]
for i in range(d8, d9):
    m = re.match(r'^\s*(\d+)\s+(%s)\s+(%s)\s+(%s)\s+(%s)\s*$' % (FLT, FLT, FLT, FLT), lines[i])
    if m:
        idx = int(m.group(1))
        for k in range(4):
            cols[k][idx] = m.group(k + 2).replace(',', '.')
for c in cols:
    assert sorted(c) == list(range(512)), len(c)
# Column 0, the perfect-reconstruction prototype, is not emitted: the decoder uses the one
# FFmpeg applies (src/pr_table.rs, recover_pr.py).
names = [('QMF_NONPERFECT', 'non-perfect reconstruction 32-band interpolation FIR'),
         ('LFE_FIR_64', 'LFE 64x interpolation FIR'),
         ('LFE_FIR_128', 'LFE 128x interpolation FIR')]
for (n, desc), c in zip(names, cols[1:]):
    w('/// ETSI TS 102 114 Annex D.8: %s.' % desc)
    w('pub static %s: [f64; 512] = [' % n)
    for k in range(0, 512, 8):
        w('    ' + ', '.join(c[j] for j in range(k, k + 8)) + ',')
    w('];')
x96 = {}
FLT2 = r'-?\d,\d+e[-+]?\d+'
for i in range(d9, d10):
    for m in re.finditer(r'(?<![\d,.e-])(\d+(?: \d{3})?)\s+(%s)' % FLT2, lines[i]):
        x96[int(m.group(1).replace(' ', ''))] = m.group(2).replace(',', '.')
assert sorted(x96) == list(range(1, 1025)), len(x96)
w('/// ETSI TS 102 114 Annex D.9: 1 024-tap sign-modified prototype FIR for the X96 64-band QMF.')
w('pub static X96_FIR: [f64; 1024] = [')
for k in range(1, 1025, 8):
    w('    ' + ', '.join(x96[j] for j in range(k, k + 8)) + ',')
w('];')

open(out_path, 'w').write('\n'.join(out) + '\n')
print('ok: %d huffman books' % len(order), file=sys.stderr)
