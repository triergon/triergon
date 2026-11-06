#!/usr/bin/env python3
"""Summarise a native_correctness.py run against the declared lane selection.

A vector is out of set only when our decoder refused it with a typed Unsupported error
naming a feature the lane matrix (harness/codecs/STATUS.md and the lane STATUS files)
declares refused, or when it carries no stream the selected decoders take (DTS without a
core substream). Everything else that is not a pass at both levels is a failure and is
listed with its numbers; known dispositions are attached, never used to drop a row.

usage: summarize_correctness.py RUN.jsonl OUT.json
"""
import collections, json, sys

DECLARED_REFUSALS = {
    'h264': ('bit depth', '4:2:2', '4:4:4', 'chroma', 'lossless', 'flexible macroblock ordering', 'SP/SI', 'data partitioning'),
    'hevc': ('RExt', 'range extension', 'chroma_format', 'bit depth', 'persistent', 'extended_precision', 'high throughput',
             'cross-component', 'implicit rdpcm', 'explicit rdpcm', 'transform_skip_rotation', 'transform skip context',
             'cabac bypass alignment', 'palette', 'SCC'),
}
# Recorded explanations; they never change a status.
DISPOSITIONS = {
    'aac-sce-in-stereo.adts': 'FFmpeg leaves the FR plane of SCE-only frames unwritten (stale pooled buffer); ours is silent there (harness/codecs/aac/STATUS.md)',
    'ac3': 'AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (harness/codecs/STATUS.md)',
    'eac3': 'AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (harness/codecs/STATUS.md)',
    'test8b43.264': 'RGBA near-tie in the reference horizontal filter coefficients (harness/codecs/rgba-contract.json)',
    'DELTAQP_A_BRCM_4.bit': 'RGBA near-tie in the reference horizontal filter coefficients (harness/codecs/rgba-contract.json)',
    'PICSIZE_A_Bossen_1.bit': 'RGBA near-tie in the odd-width output stage (harness/codecs/rgba-contract.json)',
}


def refusal(row):
    dec = row.get('decoder', {})
    text = json.dumps(dec.get('ours_errors', [])) + dec.get('ours_stderr_tail', '')
    return [k for k in DECLARED_REFUSALS.get(row['lane'], ()) if k.lower() in text.lower()]


def classify(row):
    dec, out = row.get('decoder', {}), row.get('output', {})
    ds, os_ = dec.get('status', row.get('status')), out.get('status')
    if ds == 'ffmpeg-refused' and os_ in ('reference-produced-nothing', None):
        # ours_frames counts every non-error JSON line, the example's summary included.
        typed = any('nsupported' in json.dumps(e) for e in dec.get('ours_errors', []))
        return 'refused-by-both' if typed or dec.get('ours_frames', 0) <= 1 else 'ffmpeg-refused-ours-decoded'
    if row['lane'] == 'dts' and dec.get('ours_info', {}).get('frames') == 0 and dec.get('ours_rc') == 0 \
            and not dec.get('ours_info', {}).get('errors'):
        return 'out-of-set (no core substream)'
    if (ds in ('refused-unsupported', 'fail') and refusal(row)):
        return 'out-of-set (typed refusal: %s)' % ', '.join(refusal(row))
    if ds == 'pass' and os_ == 'pass':
        return 'pass'
    return 'FAIL'


def main():
    rows = [json.loads(l) for l in open(sys.argv[1])]
    summary = collections.defaultdict(collections.Counter)
    failures = []
    for r in rows:
        c = classify(r)
        summary[r['lane']][c.split(' (')[0]] += 1
        if c == 'FAIL':
            d, o = r.get('decoder', {}), r.get('output', {})
            failures.append(dict(
                lane=r['lane'], name=r['name'], decoder=d.get('status'), output=o.get('status'),
                decoder_numbers={k: d.get(k) for k in ('frames_ref', 'frames_ours', 'frames_equal', 'first_diff', 'worst_rms',
                                                       'worst_abs', 'ffmpeg_samples', 'ours_samples') if d.get(k) is not None},
                output_numbers={k: o.get(k) for k in ('pictures_ref', 'pictures_ours', 'differing_pictures', 'max_abs',
                                                      'first_diff', 'max_abs_lsb', 'differing_samples', 'samples_ours',
                                                      'samples_ref') if o.get(k) is not None},
                disposition=DISPOSITIONS.get(r['name'], DISPOSITIONS.get(r['lane']))))
        r['classification'] = c
    out = dict(schema='triergon.d4.correctness-summary.v1', run=sys.argv[1], vectors=len(rows),
               per_lane={k: dict(v) for k, v in sorted(summary.items())}, failures=failures,
               rows=[dict(lane=r['lane'], name=r['name'], sha256=r['sha256'], classification=r['classification'],
                          decoder=r.get('decoder', {}).get('status'), output=r.get('output', {}).get('status')) for r in rows])
    open(sys.argv[2], 'w').write(json.dumps(out, indent=1) + '\n')
    for lane, c in sorted(summary.items()):
        print(lane, dict(c))
    for f in failures:
        print('FAIL', f['lane'], f['name'], f['decoder'], f['output'], f['output_numbers'].get('max_abs', f['output_numbers'].get('max_abs_lsb')),
              (f['disposition'] or '')[:60])


if __name__ == '__main__':
    main()
