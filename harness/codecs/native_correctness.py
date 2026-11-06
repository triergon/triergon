#!/usr/bin/env python3
"""Codec native correctness grader (research tooling).

Two levels per vector, graded by the frozen harness/codecs/numeric-rules.json:
  decoder   our lane example (planar YUV / interleaved f32) against the pinned FFmpeg
            decoder output (-f rawvideo -pix_fmt yuv420p|yuv420p10le, -f f32le): video
            bit-exact per frame, audio per-channel RMS / max |d| and exact counts.
  output    crates/codec-worker against harness/codecs/reference_decode (the simulator's
            filter chains): RGBA bit-exact per picture, stereo s16 within 1 LSB.
Nothing is excluded: refusals, crashes, count mismatches and FFmpeg refusals are
recorded per vector as they happen. Selection (in-set or not) is applied afterwards
from the lane matrices, never by skipping inputs here.

  manifest --out M.json LANE=DIR[,DIR...] ...      hash every vector (run where they live)
  grade --manifest M.json --root R --bin DIR --ffmpeg FF --ffprobe FP
        --codec-worker W --reference R2 --out OUT.jsonl [--only SUBSTR,...]
"""
import argparse, hashlib, json, os, subprocess, sys, tempfile, threading
from pathlib import Path

RULES = dict(rms=8.8e-06, abs=6.1035e-05, lsb=1)
VIDEO_EXT = {'h264': ('.264', '.26l', '.avc', '.h264', '.jsv', '.jvt'), 'hevc': ('.bit', '.bin')}
AUDIO_FMT = {'ac3': 'ac3', 'eac3': 'eac3', 'aac': 'aac', 'dts': 'dts'}
EXAMPLE = {'h264': 'codec-h264-decode', 'hevc': 'codec-hevc-decode', 'ac3': 'codec-ac3-decode',
           'eac3': 'codec-ac3-decode', 'aac': 'codec-aac-decode', 'dts': 'codec-dca-decode'}
OPERATION = {'h264': 'video-h264', 'hevc': 'video-hevc', 'ac3': 'audio-ac3', 'eac3': 'audio-eac3',
             'aac': 'audio-aac', 'dts': 'audio-dts'}
CANVAS = 1920 * 1080 * 4


def sha(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def manifest(a):
    rows = []
    for spec in a.lanes:
        lane, dirs = spec.split('=', 1)
        for d in dirs.split(','):
            for p in sorted(Path(d).rglob('*')):
                if not p.is_file():
                    continue
                if lane in VIDEO_EXT and p.suffix not in VIDEO_EXT[lane]:
                    continue
                if lane not in VIDEO_EXT:
                    want = {'ac3': '.ac3', 'eac3': '.eac3', 'aac': '.adts', 'dts': '.core.dts'}[lane]
                    if not p.name.endswith(want):
                        continue
                rows.append(dict(lane=lane, name=p.name, path=str(p), bytes=p.stat().st_size, sha256=sha(p)))
    Path(a.out).write_text(json.dumps(dict(schema='triergon.d4.correctness-manifest.v1', vectors=rows), indent=1) + '\n')
    print(len(rows), 'vectors')


class Frames:
    """Fixed-size records a child writes to stdout, or (side=True) to a pipe it opens as
    /dev/fd/N in place of an output path; hashed as they arrive, never stored."""

    def __init__(self, argv, size, side=False):
        self.size = size
        fds = ()
        if side:
            r, w = os.pipe()
            argv, fds = [f'/dev/fd/{w}' if x == '{out}' else x for x in argv], (w,)
        self.proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL if side else subprocess.PIPE,
                                     stderr=subprocess.PIPE, pass_fds=fds)
        if side:
            os.close(w)
        self.src = os.fdopen(r, 'rb') if side else self.proc.stdout
        self.err = []
        threading.Thread(target=lambda: self.err.append(self.proc.stderr.read()), daemon=True).start()
        self.tail = b''

    def next(self):
        buf = self.src.read(self.size)
        if len(buf) < self.size:
            self.tail = buf
            return None
        return buf

    def close(self, timeout=900):
        self.tail += self.src.read()
        self.src.close()
        rc = self.proc.wait(timeout=timeout)
        return rc, (self.err[0] if self.err else b'').decode(errors='replace')


def probe(ffprobe, path, fmt=None, audio=False):
    entries = ('stream=sample_rate,channels,channel_layout' if audio else
               'stream=width,height,pix_fmt,sample_aspect_ratio,color_range,color_space')
    argv = [ffprobe, '-v', 'error'] + (['-f', fmt] if fmt else []) + ['-select_streams', 'a:0' if audio else 'v:0',
                                                                       '-show_entries', entries, '-of', 'json', path]
    r = subprocess.run(argv, capture_output=True, text=True, timeout=120)
    try:
        return json.loads(r.stdout)['streams'][0]
    except (ValueError, KeyError, IndexError):
        return None


def json_lines(text):
    out = []
    for line in text.splitlines():
        line = line.strip()
        if line.startswith('{'):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out


def first_diff(a, b, width, height, depth):
    planes = [('Y', 0, width, height), ('U', width * height, width // 2, height // 2),
              ('V', width * height + (width // 2) * (height // 2), width // 2, height // 2)]
    for name, off, pw, ph in planes:
        pa, pb = a[off * depth:(off + pw * ph) * depth], b[off * depth:(off + pw * ph) * depth]
        if pa != pb:
            k = next(i for i in range(0, len(pa), depth) if pa[i:i + depth] != pb[i:i + depth]) // depth
            return dict(plane=name, x=k % pw, y=k // pw)
    return None


def video_decoder(a, v, path):
    s = probe(a.ffprobe, path)
    ours_argv = [str(a.bin / EXAMPLE[v['lane']]), path, '/dev/stdout' if v['lane'] == 'h264' else '-']
    if not s or 'pix_fmt' not in s:
        r = subprocess.run(ours_argv[:2] + ['/dev/null'], capture_output=True, text=True, timeout=900)
        lines = json_lines(r.stderr)
        return dict(status='ffmpeg-refused', ffmpeg='ffprobe found no decodable stream', ours_rc=r.returncode,
                    ours_frames=sum(1 for j in lines if 'error' not in j), ours_errors=[j for j in lines if 'error' in j][:3],
                    ours_stderr_tail=r.stderr[-300:])
    depth = 2 if '10' in s['pix_fmt'] else 1
    out_fmt = 'yuv420p10le' if depth == 2 else 'yuv420p'
    w, h = s['width'], s['height']
    size = w * h * 3 // 2 * depth
    ref = Frames([a.ffmpeg, '-nostdin', '-v', 'error', '-threads', '1', '-i', path, '-f', 'rawvideo', '-pix_fmt', out_fmt,
                  '-fps_mode', 'passthrough', '-'], size)
    ours = Frames(ours_argv, size)
    n = match = 0
    first = None
    count_ref = count_ours = 0
    while True:
        fa, fb = ours.next(), ref.next()
        count_ours += fa is not None
        count_ref += fb is not None
        if fa is None or fb is None:
            break
        if fa == fb:
            match += 1
        elif first is None:
            first = dict(frame=n, **(first_diff(fa, fb, w, h, depth) or {}))
        n += 1
    while ours.next() is not None:
        count_ours += 1
    while ref.next() is not None:
        count_ref += 1
    rc_o, err_o = ours.close()
    rc_r, err_r = ref.close()
    lines = json_lines(err_o)
    errors = [j for j in lines if 'error' in j]
    unsupported = [j for j in errors if 'nsupported' in json.dumps(j)]
    if count_ref == 0 and rc_r != 0:
        status = 'ffmpeg-refused'
    elif unsupported and count_ours == 0:
        status = 'refused-unsupported'
    elif first is None and count_ours == count_ref and rc_o == 0 and not ours.tail:
        status = 'pass'
    else:
        status = 'fail'
    return dict(status=status, frames_ref=count_ref, frames_ours=count_ours, frames_equal=match, first_diff=first,
                width=w, height=h, pix_fmt=s['pix_fmt'], ours_rc=rc_o, ffmpeg_rc=rc_r, ours_errors=errors[:3],
                ours_partial_bytes=len(ours.tail), ffmpeg_stderr_tail=err_r[-300:])


def channel_stats(cand, ref, channels):
    import numpy as np
    c = np.frombuffer(cand, dtype='<f4')
    r = np.frombuffer(ref, dtype='<f4')
    if channels <= 0 or c.size != r.size or c.size % channels:
        return None
    d = (c.astype(np.float64) - r.astype(np.float64)).reshape(-1, channels)
    rms = np.sqrt(np.mean(d * d, axis=0)) if d.size else np.zeros(channels)
    mx = np.max(np.abs(d), axis=0) if d.size else np.zeros(channels)
    return dict(rms=[float(x) for x in rms], abs=[float(x) for x in mx])


def audio_decoder(a, v, path, tmp):
    fmt = AUDIO_FMT[v['lane']]
    s = probe(a.ffprobe, path, fmt, audio=True)
    ref_path, ours_path = tmp / 'ref.f32', tmp / 'ours.f32'
    rr = subprocess.run([a.ffmpeg, '-nostdin', '-v', 'error', '-threads', '1', '-f', fmt, '-i', path, '-f', 'f32le',
                         '-c:a', 'pcm_f32le', '-y', str(ref_path)], capture_output=True, text=True, timeout=900)
    ro = subprocess.run([str(a.bin / EXAMPLE[v['lane']]), path, str(ours_path)], capture_output=True, text=True, timeout=900)
    info = (json_lines(ro.stderr) or [{}])[-1]
    ours_ch = info.get('channels')
    ours_rate = info.get('sample_rate', info.get('rate'))
    ref = ref_path.read_bytes() if ref_path.exists() else b''
    cand = ours_path.read_bytes() if ours_path.exists() else b''
    ref_ch = int(s['channels']) if s and s.get('channels') else None
    ref_rate = int(s['sample_rate']) if s and s.get('sample_rate') else None
    row = dict(ffmpeg_rate=ref_rate, ours_rate=ours_rate, ffmpeg_channels=ref_ch, ours_channels=ours_ch,
               ffmpeg_layout=s.get('channel_layout') if s else None, ours_layout=info.get('layout'),
               ffmpeg_samples=len(ref) // 4 // ref_ch if ref_ch else None,
               ours_samples=len(cand) // 4 // ours_ch if ours_ch else None, ours_rc=ro.returncode, ffmpeg_rc=rr.returncode,
               ours_info=info, ffmpeg_stderr_tail=rr.stderr[-300:])
    if not ref:
        row['status'] = 'ffmpeg-refused'
        return row
    if 'nsupported' in ro.stderr and not cand:
        row['status'] = 'refused-unsupported'
        row['ours_stderr_tail'] = ro.stderr[-300:]
        return row
    exact = ours_ch == ref_ch and ours_rate == ref_rate and len(cand) == len(ref)
    st = channel_stats(cand, ref, ref_ch) if exact else None
    row.update(stats=st, worst_rms=max(st['rms']) if st else None, worst_abs=max(st['abs']) if st else None)
    row['status'] = 'pass' if st and row['worst_rms'] <= RULES['rms'] and row['worst_abs'] <= RULES['abs'] else 'fail'
    return row


def output_path(a, v, path, tmp):
    op = OPERATION[v['lane']] + '-raw'
    if v['lane'] in VIDEO_EXT:
        ours = Frames([str(a.codec_worker), op, path, '{out}'], CANVAS, side=True)
        ref = Frames([str(a.reference), op, path, '{out}'], CANVAS, side=True)
        n = match = 0
        first = None
        worst = 0
        diff_frames = 0
        co = cr = 0
        while True:
            fa, fb = ours.next(), ref.next()
            co += fa is not None
            cr += fb is not None
            if fa is None or fb is None:
                break
            if fa == fb:
                match += 1
            else:
                import numpy as np
                d = np.abs(np.frombuffer(fa, np.uint8).astype(np.int16) - np.frombuffer(fb, np.uint8).astype(np.int16))
                worst = max(worst, int(d.max()))
                diff_frames += 1
                if first is None:
                    k = int(np.argmax(d != 0))
                    first = dict(frame=n, x=(k // 4) % 1920, y=(k // 4) // 1920, channel='RGBA'[k % 4],
                                 differing_bytes=int(np.count_nonzero(d)))
            n += 1
        while ours.next() is not None:
            co += 1
        while ref.next() is not None:
            cr += 1
        rc_o, err_o = ours.close()
        rc_r, err_r = ref.close()
        status = ('reference-produced-nothing' if cr == 0 else
                  'pass' if first is None and co == cr else 'fail')
        return dict(status=status, pictures_ref=cr, pictures_ours=co, pictures_equal=match, differing_pictures=diff_frames,
                    max_abs=worst, first_diff=first, ours_rc=rc_o, reference_rc=rc_r, ours_stderr_tail=err_o[-300:],
                    reference_stderr_tail=err_r[-300:])
    o, r = tmp / 'ours.s16', tmp / 'ref.s16'
    po = subprocess.run([str(a.codec_worker), op, path, str(o)], capture_output=True, text=True, timeout=900)
    pr = subprocess.run([str(a.reference), op, path, str(r)], capture_output=True, text=True, timeout=900)
    co, cr = (o.read_bytes() if o.exists() else b''), (r.read_bytes() if r.exists() else b'')
    row = dict(samples_ours=len(co) // 4, samples_ref=len(cr) // 4, ours_rc=po.returncode, reference_rc=pr.returncode,
               ours_stderr_tail=po.stderr[-300:], reference_stderr_tail=pr.stderr[-300:])
    if not cr:
        row['status'] = 'reference-produced-nothing'
        return row
    if len(co) != len(cr):
        row['status'] = 'fail'
        return row
    import numpy as np
    d = np.abs(np.frombuffer(co, '<i2').astype(np.int32) - np.frombuffer(cr, '<i2').astype(np.int32))
    row.update(max_abs_lsb=int(d.max()) if d.size else 0, differing_samples=int(np.count_nonzero(d)),
               exact=bool(not d.any()))
    row['status'] = 'pass' if row['max_abs_lsb'] <= RULES['lsb'] else 'fail'
    return row


def grade(a):
    vectors = json.loads(Path(a.manifest).read_text())['vectors']
    only = a.only.split(',') if a.only else None
    a.bin = Path(a.bin)
    with open(a.out, 'a') as out:
        for v in vectors:
            if only and not any(o in v['name'] or o == v['lane'] for o in only):
                continue
            path = str(Path(a.root) / v['path'])
            row = dict(lane=v['lane'], name=v['name'], path=v['path'], sha256=v['sha256'])
            if sha(path) != v['sha256']:
                row['status'] = 'input-hash-mismatch'
            else:
                with tempfile.TemporaryDirectory(dir=a.scratch) as t:
                    t = Path(t)
                    try:
                        row['decoder'] = (video_decoder(a, v, path) if v['lane'] in VIDEO_EXT
                                          else audio_decoder(a, v, path, t))
                    except subprocess.TimeoutExpired as e:
                        row['decoder'] = dict(status='timeout', argv=e.cmd)
                    try:
                        (t / 'o').mkdir()
                        row['output'] = output_path(a, v, path, t / 'o')
                    except subprocess.TimeoutExpired as e:
                        row['output'] = dict(status='timeout', argv=e.cmd)
            out.write(json.dumps(row) + '\n')
            out.flush()
            print(v['lane'], v['name'], row.get('decoder', {}).get('status', row.get('status')),
                  row.get('output', {}).get('status', ''), flush=True)
    print('DONE')


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest='cmd', required=True)
    m = sub.add_parser('manifest')
    m.add_argument('--out', required=True)
    m.add_argument('lanes', nargs='+')
    g = sub.add_parser('grade')
    for k in ('manifest', 'root', 'bin', 'ffmpeg', 'ffprobe', 'codec-worker', 'reference', 'out', 'scratch'):
        g.add_argument('--' + k, required=k != 'scratch')
    g.add_argument('--only')
    a = p.parse_args()
    a.codec_worker = Path(getattr(a, 'codec_worker', '') or '.')
    (manifest if a.cmd == 'manifest' else grade)(a)


if __name__ == '__main__':
    sys.exit(main())
