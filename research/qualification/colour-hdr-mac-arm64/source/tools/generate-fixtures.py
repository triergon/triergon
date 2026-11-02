#!/usr/bin/env python3
"""Generate tiny synthetic fixtures; never fetch or commit household media."""
import argparse,hashlib,json,pathlib,subprocess
p=argparse.ArgumentParser();p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args();a.output.mkdir(parents=True,exist_ok=True)
def run(args):subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-nostdin','-y',*args],check=True,timeout=60)
srt=a.output/'captions.srt';srt.write_text('1\n00:00:00,500 --> 00:00:01,200\nSynthetic caption\n\n2\n00:00:02,000 --> 00:00:03,000\nSecond caption\n')
avc=a.output/'avc-aac-sub.mkv'
run(['-f','lavfi','-i','testsrc2=size=160x96:rate=12','-f','lavfi','-i','sine=frequency=440:sample_rate=48000','-i',str(srt),'-t','4','-map','0:v','-map','1:a','-map','2:s','-c:v','libx264','-threads','1','-g','12','-bf','2','-pix_fmt','yuv420p','-c:a','aac','-b:a','64k','-c:s','srt',str(avc)])
for name,args in [('avc-aac.mp4',[]),('avc-aac-fragmented.mp4',['-movflags','frag_keyframe+empty_moov']),('avc-aac.ts',['-bsf:v','h264_mp4toannexb'])]:
 run(['-i',str(avc),'-map','0:v','-map','0:a','-c','copy',*args,str(a.output/name)])
run(['-f','lavfi','-i','testsrc2=size=160x96:rate=12','-f','lavfi','-i','sine=frequency=440:sample_rate=48000','-t','4','-c:v','libx265','-threads','1','-x265-params','pools=none:frame-threads=1:log-level=error:keyint=12:bframes=2','-c:a','aac','-b:a','64k',str(a.output/'hevc-aac.mkv')])
rows=[]
for path in sorted(a.output.iterdir()):
 if path.suffix not in ['.mkv','.mp4','.ts','.srt']:continue
 b=path.read_bytes();rows.append(dict(name=path.name,bytes=len(b),sha256=hashlib.sha256(b).hexdigest(),origin='synthetic lavfi test signals / authored test captions'))
(a.output/'manifest.json').write_text(json.dumps({'ffmpeg_version':subprocess.check_output(['ffmpeg','-version'],text=True).splitlines()[0],'files':rows},indent=2)+'\n')
print(json.dumps(rows,indent=2))
