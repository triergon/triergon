#!/usr/bin/env python3
"""Compare triergon-codec-hevc output against the FFmpeg binary (black-box oracle).

Streaming: both decoders write raw frames to a pipe; only per-frame MD5 lists are kept
(target/agent-hevc/ref/<name>.hashes), never full YUV files. On a mismatch both outputs are
re-streamed up to the first differing frame to locate plane / position.

usage: python3 -I compare.py [vector-name-substring | /abs/path ...]
"""
import hashlib, json, os, subprocess, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..'))
VEC = os.path.join(ROOT, '.research/fate-samples/hevc-conformance')
SCR = os.path.join(ROOT, 'target/agent-hevc/ref')
DEC = os.path.join(ROOT, 'target/agent-hevc/release/examples/decode')
FF = '/opt/homebrew/bin/ffmpeg'
FFP = '/opt/homebrew/bin/ffprobe'
os.makedirs(SCR, exist_ok=True)

def probe(path):
    r = subprocess.run([FFP, '-v', 'error', '-select_streams', 'v:0', '-show_entries',
                        'stream=width,height,pix_fmt,sample_aspect_ratio,color_range,color_primaries,color_transfer,color_space',
                        '-of', 'json', path], capture_output=True, text=True)
    try:
        return json.loads(r.stdout)['streams'][0]
    except Exception:
        return None

def ff_cmd(path, out_fmt):
    return [FF, '-v', 'error', '-threads', '1', '-i', path, '-f', 'rawvideo', '-pix_fmt', out_fmt,
            '-fps_mode', 'passthrough', '-']

def stream_frames(cmd, fsz, limit=None):
    """Yield raw frames from a subprocess stdout."""
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    n = 0
    try:
        while limit is None or n < limit:
            buf = p.stdout.read(fsz)
            if len(buf) < fsz:
                break
            n += 1
            yield buf
    finally:
        p.stdout.close()
        p.kill() if limit is not None else None
        err = p.stderr.read().decode(errors='replace')
        p.wait()
        stream_frames.last_stderr = err
        stream_frames.last_rc = p.returncode

def reference(name, path):
    meta = os.path.join(SCR, name + '.json')
    hfile = os.path.join(SCR, name + '.hashes')
    if os.path.exists(meta) and os.path.exists(hfile):
        return json.load(open(meta)), open(hfile).read().split()
    s = probe(path)
    if s is None or 'pix_fmt' not in s:
        m = {'error': 'ffprobe failed'}
        json.dump(m, open(meta, 'w')); open(hfile, 'w').write('')
        return m, []
    pf = s['pix_fmt']
    out_fmt = 'yuv420p10le' if '10' in pf else 'yuv420p'
    b = 2 if '10' in out_fmt else 1
    fsz = s['width'] * s['height'] * 3 // 2 * b
    hashes = [hashlib.md5(f).hexdigest() for f in stream_frames(ff_cmd(path, out_fmt), fsz)]
    m = {'width': s['width'], 'height': s['height'], 'pix_fmt': pf, 'out_fmt': out_fmt, 'bytes': b,
         'ffmpeg_stderr': stream_frames.last_stderr[-500:], 'stream': s}
    json.dump(m, open(meta, 'w'))
    open(hfile, 'w').write('\n'.join(hashes) + ('\n' if hashes else ''))
    return m, hashes

def locate(path, m, i):
    w, h, b = m['width'], m['height'], m['bytes']
    fsz = w * h * 3 // 2 * b
    a = c = None
    for k, f in enumerate(stream_frames([DEC, path, '-', '--quiet'], fsz, i + 1)):
        a = f
    for k, f in enumerate(stream_frames(ff_cmd(path, m['out_fmt']), fsz, i + 1)):
        c = f
    if a is None or c is None:
        return ''
    planes = [(0, w, h), (w * h, w // 2, h // 2), (w * h + (w // 2) * (h // 2), w // 2, h // 2)]
    for pi, (off, pw, ph) in enumerate(planes):
        pa = a[off * b:(off + pw * ph) * b]; pc = c[off * b:(off + pw * ph) * b]
        if pa != pc:
            for k in range(0, len(pa), b):
                if pa[k:k + b] != pc[k:k + b]:
                    px = (k // b) % pw; py = (k // b) // pw
                    sc = 1 if pi == 0 else 2
                    return f' plane={"YUV"[pi]} x={px} y={py} ctb64=({px*sc//64},{py*sc//64})'
    return ''

def run(name, path):
    m, ref = reference(name, path)
    r_frames = []
    ours = []
    if 'error' not in m:
        fsz = m['width'] * m['height'] * 3 // 2 * m['bytes']
        ours = [hashlib.md5(f).hexdigest() for f in stream_frames([DEC, path, '-'], fsz)]
        err = stream_frames.last_stderr
    else:
        p = subprocess.run([DEC, path, '/dev/null'], capture_output=True, text=True, timeout=600)
        err = p.stderr
    lines = [l for l in err.splitlines() if l.startswith('{')]
    frames = [json.loads(l) for l in lines if '"frame"' in l]
    errs = [json.loads(l) for l in lines if '"error"' in l]
    if not ref:
        return f'{name}: NOREF ours={len(frames)} errs={errs[:1]} ffmpeg={m.get("ffmpeg_stderr","")[:120]!r}'
    if frames and (frames[0]['width'] != m['width'] or frames[0]['height'] != m['height']):
        return f'{name}: SIZE ours={frames[0]["width"]}x{frames[0]["height"]} ref={m["width"]}x{m["height"]}'
    first_bad = next((i for i in range(min(len(ours), len(ref))) if ours[i] != ref[i]), None)
    status = 'PASS' if first_bad is None and len(ours) == len(ref) and not errs else 'FAIL'
    detail = ''
    if first_bad is not None:
        poc = frames[first_bad]['poc'] if first_bad < len(frames) else '?'
        detail += f' first_diff frame={first_bad} poc={poc}' + locate(path, m, first_bad)
    if len(ours) != len(ref):
        detail += f' count ours={len(ours)} ref={len(ref)}'
    if errs:
        detail += f' errors={len(errs)} first={errs[0]}'
    return f'{name}: {status} frames={len(ref)}{detail}'

def main():
    sel = sys.argv[1:]
    items = []
    for a in sel:
        if os.path.isabs(a):
            items.append((os.path.basename(a), a))
    names = sorted(os.listdir(VEC))
    pats = [a for a in sel if not os.path.isabs(a)]
    if pats or not sel:
        items += [(n, os.path.join(VEC, n)) for n in names if not pats or any(p in n for p in pats)]
    for n, p in items:
        try:
            print(run(n, p), flush=True)
        except subprocess.TimeoutExpired:
            print(f'{n}: TIMEOUT', flush=True)

if __name__ == '__main__':
    main()
