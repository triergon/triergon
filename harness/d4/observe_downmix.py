#!/usr/bin/env python3
"""Native observation of the simulator's `-ac 2` device path (research tooling).

For each audio vector: the decoder's native float output, the CLI's stereo s16
output (`-vn -ac 2 -f s16le`, what the pulse/alsa muxers receive) and the
resampler's reported mix levels and matrix. Writes one JSON record per vector.
"""
import json, re, subprocess, sys
from pathlib import Path
ffmpeg, out_dir = sys.argv[1], Path(sys.argv[2]); out_dir.mkdir(parents=True, exist_ok=True)
records = []
for spec in sys.argv[3:]:
    fmt, path = spec.split(':', 1)
    log = subprocess.run([ffmpeg, '-hide_banner', '-v', 'debug', '-f', fmt, '-i', path, '-vn', '-ac', '2', '-f', 's16le', '-y',
                          str(out_dir / (Path(path).name + '.s16'))], capture_output=True, text=True).stderr
    subprocess.run([ffmpeg, '-hide_banner', '-v', 'error', '-f', fmt, '-i', path, '-vn', '-f', 'f32le', '-c:a', 'pcm_f32le', '-y',
                    str(out_dir / (Path(path).name + '.f32'))], check=False)
    probe = subprocess.run([str(Path(ffmpeg).with_name('ffprobe')), '-v', 'error', '-f', fmt, '-show_entries',
                            'stream=sample_rate,channels,channel_layout,codec_name', '-of', 'json', path], capture_output=True, text=True).stdout
    levels = re.findall(r'Mix levels: (.*)', log)
    matrix = [l.split('] ')[-1] for l in log.splitlines() if re.search(r'\] (FL|FR): ', l)]
    records.append(dict(vector=path, format=fmt, stream=json.loads(probe or '{}'), mix_levels=levels[:1], matrix=matrix[:2],
                        swr_lines=[l.split('] ')[-1] for l in log.splitlines() if 'SWR' in l or 'aresample' in l][:12]))
    print(Path(path).name, levels[:1], matrix[:2], flush=True)
(out_dir / 'OBSERVATIONS.json').write_text(json.dumps(records, indent=1) + '\n')
