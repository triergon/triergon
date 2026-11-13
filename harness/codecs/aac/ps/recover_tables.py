"""Rerun the whole Parametric Stereo table recovery against the pinned reference decoder
and write every recovered table as JSON for `cargo xtask codecs aac-ps-tables`.

Steps (each a scripted probe chain of its own, see harness/codecs/aac/ps/README.md):
1. the PS Huffman books (recover_huff.py, recover_ipdopd.py) and SBR_F_ENVBAL30
   (recover_sbr_envbal.py);
2. the signal side (signal/*/recover_*.py): the hybrid filterbank and band maps, the
   decorrelator and its ducking, the mixing and IPD/OPD, the behaviour over time. Each
   writes its fits and the checks of its model against fresh reference output;
3. the tables: fitted values written with the fewest decimals that stay within the
   precision their fit established, structures as the probes found them. Any failed
   check stops the run.

FFMPEG (the pinned FFmpeg 9.0.2 decode build) and PS_INPUT (this decoder's ps_input
example) come from the environment.

usage: recover_tables.py WORKDIR OUT.json
"""
import json
import math
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SIGNAL = os.path.join(HERE, 'signal')


def run(args, work, env=None):
    log = open(os.path.join(work, 'log.txt'), 'a')
    log.write('$ %s\n' % ' '.join(args))
    log.flush()
    e = dict(os.environ, **(env or {}))
    r = subprocess.run([sys.executable, '-s'] + args, cwd=os.path.dirname(args[0]), env=e,
                       stdout=log, stderr=subprocess.STDOUT)
    if r.returncode:
        raise SystemExit('%s failed (see %s)' % (args[0], log.name))


def books(work):
    w = os.path.join(work, 'books')
    os.makedirs(w, exist_ok=True)
    out = {}
    for book in ('iid_df', 'iid_df_fine', 'icc_df', 'iid_dt', 'iid_dt_fine', 'icc_dt'):
        o = os.path.join(w, book + '.json')
        args = [os.path.join(HERE, 'recover_huff.py'), book, o, os.path.join(w, book)]
        if '_dt' in book:
            args.append(os.path.join(w, book.replace('_dt', '_df') + '.json'))
        if not os.path.exists(o):
            run(args, work)
        out[book] = json.load(open(o))
    o = os.path.join(w, 'ipdopd.json')
    if not os.path.exists(o):
        run([os.path.join(HERE, 'recover_ipdopd.py'), o, os.path.join(w, 'ipdopd')], work)
    out['ipdopd'] = json.load(open(o))
    o = os.path.join(w, 'envbal.json')
    if not os.path.exists(o):
        run([os.path.join(HERE, 'recover_sbr_envbal.py'), o, os.path.join(w, 'envbal')], work)
    out['envbal'] = json.load(open(o))
    res = []
    docs = {
        'IID': 'IID, coarse grid (symbol = value + 14)',
        'IID_FINE': 'IID, fine grid (symbol = value + 30)',
        'ICC': 'ICC (symbol = value + 7)',
        'IPD': 'IPD (symbol = value, modulo 8)',
        'OPD': 'OPD (symbol = value, modulo 8)',
    }
    for name, key, off in [('IID_DF', 'iid_df', 14), ('IID_DT', 'iid_dt', 14),
                           ('IID_DF_FINE', 'iid_df_fine', 30), ('IID_DT_FINE', 'iid_dt_fine', 30),
                           ('ICC_DF', 'icc_df', 7), ('ICC_DT', 'icc_dt', 7)]:
        m = {v + off: c for c, v in out[key]['codewords'].items()}
        kind = name.replace('_DF', '').replace('_DT', '')
        res.append((name, m, '%s, %s.' % (docs[kind], 'time differential' if '_DT' in name
                                         else 'frequency differential')))
    for b in ('ipd_df', 'ipd_dt', 'opd_df', 'opd_dt'):
        m = {v: c for c, v in out['ipdopd']['books'][b].items()}
        res.append((b.upper(), m, '%s, %s.' % (docs[b[:3].upper()], 'time differential'
                                              if b.endswith('dt') else 'frequency differential')))
    m = {int(v) + 12: c for c, v in out['envbal']['codewords'].items()}
    res.append(('SBR_F_ENVBAL30', m, 'SBR envelope balance at 3.0 dB steps, frequency differential '
                '(symbol = value + 12).'))
    books = []
    for name, m, doc in res:
        n = len(m)
        if sorted(m) != list(range(n)):
            raise SystemExit('%s: symbols do not cover 0..%d' % (name, n - 1))
        kraft = sum(2.0 ** -len(c) for c in m.values())
        if kraft != 1.0:
            raise SystemExit('%s: not a complete prefix code (Kraft sum %r)' % (name, kraft))
        books.append({'name': name, 'doc': doc,
                      'codes': [[int(m[s], 2), len(m[s])] for s in range(n)]})
    return books


def signal(work):
    out = {}
    env = {'PYTHONPATH': SIGNAL}
    for lane, script, name in (('hyb', 'recover_hyb.py', 'hyb.json'),
                               ('dec', 'recover_dec.py', 'dec.json'),
                               ('mix', 'recover_mix.py', 'mix.json'),
                               ('time', 'recover_time.py', 'time.json')):
        w = os.path.join(work, lane)
        os.makedirs(w, exist_ok=True)
        o = os.path.join(w, name)
        if not os.path.exists(o):
            run([os.path.join(SIGNAL, lane, script), w], work, env)
        out[lane] = json.load(open(o))
    return out


def snap(v, tol, dmax=12):
    """The decimal with the fewest digits within tol of v (no exponent)."""
    for d in range(dmax + 1):
        r = round(v, d) + 0.0  # no negative zero
        if abs(r - v) <= tol:
            s = '%.*f' % (d, r)
            return s if '.' in s else s + '.0'
    raise SystemExit('cannot write %r within %r' % (v, tol))


def tol_text(tol):
    return ('%.0e' % tol).replace('e-0', 'e-')


def need(cond, what):
    if not cond:
        raise SystemExit('check failed: ' + what)


def tables(sig):
    hyb, dec, mix, tim = sig['hyb'], sig['dec'], sig['mix'], sig['time']
    out = []

    def table(name, ty, doc, items=None, value=None, precision=0.0):
        t = {'name': name, 'type': ty, 'doc': doc, 'precision': precision}
        if items is not None:
            t['items'] = items
        else:
            t['value'] = value
        out.append(t)

    # 1. hybrid prototypes
    table1 = {8: ['0.125', '0.11793710567217', '0.09885108575264', '0.07266113929591',
                  '0.04546865930473', '0.02270420949825', '0.00746082949812']}
    tol = 1e-8
    for cfg, q, key in ((20, 8, 'PROTO_20_Q8'), (20, 4, 'PROTO_20_Q4'), (34, 12, 'PROTO_34_Q12'),
                        (34, 8, 'PROTO_34_Q8'), (34, 4, 'PROTO_34_Q4')):
        fit = hyb['proto_%d' % cfg][str(q)]['n6_12']
        items = []
        for n, v in enumerate(fit):
            if (cfg, q) == (20, 8):
                # TS 26.405 Table 1 g0, confirmed where an output depends on the tap
                need(v is None or abs(v - float(table1[8][n])) <= tol, '%s[%d] = Table 1' % (key, n))
                items.append(table1[8][n])
            elif v is None:
                items.append('0.0')  # no reference output depends on the tap
            elif n == 0:
                need(abs(v - 1 / q) <= tol, '%s centre tap 1/Q' % key)
                items.append('1.0 / %d.0' % q)
            else:
                items.append(snap(v, tol / 2))
        doc = {
            'PROTO_20_Q8': 'Hybrid prototypes `g[n]`, n = 6..12 (`g[12 - n] = g[n]`), by configuration and split.\n'
                           '20 bands, Q = 8: 3GPP TS 26.405 Table 1 g0, confirmed by the fits.',
            'PROTO_20_Q4': '20 bands, Q = 4 (fitted; the output departs from TS 26.405 Table 1 g1,2 by\n'
                           'up to 8e-3). No reference output depends on taps n = 8 and 12; they are 0.',
            'PROTO_34_Q12': '34 bands, Q = 12, 8 and 4 (fitted).',
        }.get(key, '34 bands (fitted).')
        table(key, '[f64; 7]', doc + ' Precision %s.' % tol_text(tol), items, precision=tol)

    # 2. band layouts: split bands with the decorrelator frequency and the image flag
    image = {c: {tuple(x) for x in mix['D_image_subbands'][str(c)]['image_subbands']} for c in (20, 34)}
    for c in (20, 34):
        items = []
        for b in dec['low_bands'][str(c)]:
            mask = sum(1 << m for m in b['subbands'])
            img = any((b['qmf'], m) in image[c] for m in b['subbands'])
            items.append('(%d, %d, 0x%03x, %d, %r, %s)' % (b['qmf'], b['Q'], mask, b['par'], b['f'],
                                                          'true' if img else 'false'))
        doc = ('One hybrid band of a split QMF band: (QMF band, split Q, sub-band mask, parameter band,\n'
               'decorrelator frequency in QMF band widths, image band). An image band holds a\n'
               'negative-frequency sub-band; its matrix ramps start from the conjugated stored matrix.\n'
               if c == 20 else '') + ('20 bands: QMF 0..2 split 8/4/4.' if c == 20 else
                                      '34 bands: QMF 0..4 split 12/8/4/4/4, every sub-band its own band.')
        table('LOW_%d' % c, '[(u8, u8, u16, u8, f64, bool); %d]' % len(items), doc, items)
    for c in (20, 34):
        qp = {int(k): v for k, v in hyb['layout_%d' % c]['qmf_par'].items()}
        need(qp == {int(k): v for k, v in dec['qmf_par'][str(c)].items()}, 'qmf_par %d agrees' % c)
        starts = []
        for k in sorted(qp):
            if not starts or starts[-1][1] != qp[k]:
                starts.append((k, qp[k]))
        table('QMF_PAR_%d' % c, '[(u8, u8); %d]' % len(starts),
              'First QMF band of each parameter band above the split bands: (QMF band, parameter band).'
              if c == 20 else '34 bands.', ['(%d, %d)' % s for s in starts])
    items = []
    for m in hyb['maps']['iid_20_to_34']:
        if isinstance(m, int):
            items.append('(%d, %d)' % (m, m))
        else:
            need(m[0][2] == 'trunc0', '20 -> 34 pair rule')
            items.append('(%d, %d)' % (m[0][0], m[0][1]))
    table('MAP_20_34', '[(u8, u8); 34]', '20-band parameters on the 34-band configuration: configured band j takes the coded\n'
          'bands (a, b), the index mean truncated toward zero (a = b: a copy).', items)

    # 3. decorrelator
    k = dec['constants']
    precision = {'A': 2e-8, 'QF': 1e-8, 'F0': 2e-8, 'SLOPE': 3e-9, 'ALPHA': 2e-8, 'BETA': 5e-8, 'GAMMA': 1e-6}
    table('DECAY', '[f64; 3]', 'Decorrelator all-pass coefficients a_m (precision 2e-8).',
          [snap(v, precision['A']) for v in k['A']], precision=precision['A'])
    table('FRACT', '[f64; 3]', 'Fractional delays of the sections (1e-8).', [snap(v, precision['QF']) for v in k['QF']],
          precision=precision['QF'])
    table('FRACT0', 'f64', 'Fractional delay of the lead delay (2e-8).', value=snap(k['F0'], precision['F0']),
          precision=precision['F0'])
    table('SLOPE', 'f64', 'Decrease of the coefficients per QMF band above the split bands (3e-9).',
          value=snap(k['SLOPE'], precision['SLOPE']), precision=precision['SLOPE'])
    table('SECTION_DELAY', '[usize; 3]', 'Section delays in slots.', ['%d' % d for d in k['DELAYS']])
    table('LEAD_DELAY', 'usize', 'The lead delay.', value='%d' % k['PRE'])
    table('LONG_DELAY', 'usize', 'The plain delay from `LONG_START`.', value='%d' % k['LONG'])
    need(k['SHORT'] == 1, 'one-slot delay above SHORT_START')
    table('LONG_START', 'usize', 'First QMF band of the 14-slot delay.', value='%d' % k['AP_END'])
    table('SHORT_START', 'usize', 'First QMF band of the one-slot delay.', value='%d' % k['SHORT_START'])
    table('PEAK_DECAY', 'f64', 'Transient ducking: peak decay (2e-8).', value=snap(k['ALPHA'], precision['ALPHA']),
          precision=precision['ALPHA'])
    table('SMOOTH', 'f64', 'Smoothing (5e-8).', value=snap(k['BETA'], precision['BETA']), precision=precision['BETA'])
    table('DUCK_LIMIT', 'f64', 'Threshold (fit precision 3e-7).', value=snap(k['GAMMA'], precision['GAMMA']),
          precision=precision['GAMMA'])

    # 4. mixing
    lv = mix['B_matrices']['A_coarse']['iid_level_db_per_index']
    table('IID_COARSE', '[f64; 8]', 'IID grids in dB (indices 0..7 and 0..15, negative indices mirror).',
          [snap(v, 1e-5) for v in lv], precision=1e-5)
    lv = mix['B_matrices']['A_fine']['iid_level_db_per_index']
    table('IID_FINE', '[f64; 16]', 'Fine grid.', [snap(v, 1e-5) for v in lv], precision=1e-5)
    table('ICC', '[f64; 8]', 'ICC values per index (precision 1e-7).', [snap(v, 1e-7) for v in mix['icc_rho']],
          precision=1e-7)
    table('ICC_FLOOR_B', 'f64', 'Floor of the ICC value in the second mixing procedure (1e-7).',
          value=snap(mix['rho_floor_second_procedure'], 1e-7), precision=1e-7)
    w = mix['C_phase_time']['smoothing_weights_rel_current']
    table('PHASE_WEIGHTS', '[f64; 3]', 'IPD/OPD phase smoothing weights of the current and two previous envelopes (1e-7).',
          [snap(v, 1e-7) for v in w], precision=1e-7)
    for v in mix['C_phase_time']['phase_step_from_steady']:
        need(abs(v - math.pi / 4) < 1e-7, 'IPD/OPD step pi/4')

    # 5. behaviour over time
    conv = tim['conversion']['mean_34_to_20']
    table('CONV_34_20', '[&[u8]; 20]', 'Start matrices of a frame whose configuration differs from the previous one. 34 -> 20:\n'
          'band b takes the mean of the listed 34-band rows (a row listed twice counts twice);\n'
          '20 -> 34 takes the rows of `MAP_20_34`.', ['&[%s]' % ', '.join('%d' % r for r in conv[str(b)])
                                                       for b in range(20)])
    c2 = tim['conversion']['copy_or_mean_20_to_34']
    for j, m in enumerate(hyb['maps']['iid_20_to_34']):
        want = [m, m] if isinstance(m, int) else m[0][:2]
        need(sorted(set(c2[str(j)])) == sorted(set(want)), '20 -> 34 start conversion is MAP_20_34')
    return out


def main():
    work, out = sys.argv[1], sys.argv[2]
    os.makedirs(work, exist_ok=True)
    b = books(work)
    s = signal(work)
    res = {
        'books_doc': 'Huffman books recovered from the reference decoder\'s behaviour by the probes in\n'
                     '`harness/codecs/aac/ps` (generated by `cargo xtask codecs aac-ps-tables`; do not edit).',
        'books': b,
        'tables_doc': 'Parametric Stereo signal tables recovered from the reference decoder\'s output by the\n'
                      'probes in `harness/codecs/aac/ps` (generated by `cargo xtask codecs aac-ps-tables`;\n'
                      'do not edit). Fitted values carry the precision their recovery established.',
        'tables': tables(s),
    }
    json.dump(res, open(out, 'w'), indent=1)


if __name__ == '__main__':
    main()
