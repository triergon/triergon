#!/bin/zsh
# Reproduce the bsid 9/10 rows of research/d4/ac3/STATUS.md (development oracle only).
# Run from the worktree root after
#   CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/agent-ac3b cargo build --release -p triergon-codec-ac3 --examples
# Needs FFmpeg 9.0.2 on PATH, numpy in $PYTHON, and the elementary streams of run_vectors.sh in
# target/agent-ac3/es and target/agent-ac3/syn. Media and outputs stay under target/.
set -e
R=$PWD/research/d4/ac3
X=$PWD/target/agent-ac3b/release/examples
mkdir -p target/agent-ac3b/rrv && cd target/agent-ac3b/rrv
cmp() { # ours ref nch label
  ${PYTHON:-python3} -I -c "
import sys, numpy as np
a, b = (np.fromfile(sys.argv[i], '<f4').astype(np.float64) for i in (1, 2)); n = int(sys.argv[3])
if len(a) != len(b): print(sys.argv[4], 'LEN', len(a) // n, len(b) // n); sys.exit()
d = (a - b).reshape(-1, n)
print('| %s | %d | rms %.1e | max %.1e |' % (sys.argv[4], len(a) // n, np.sqrt((d ** 2).mean(0)).max(), abs(d).max()))
" $1 $2 $3 $4
}
# 1. Self-authored probe streams (all-zero mantissas expose the bit allocation).
for acmod in 1 2; do for fs in 0 1 2; do for b in 8 9 10; do
  n=p_a${acmod}_f${fs}_b$b
  python3 -I $R/gen_reduced_rate.py $n.ac3 $b $fs 200 $((fs * 100 + b + 7 * (acmod - 1))) $acmod
  ffmpeg -nostdin -v error -y -i $n.ac3 -f f32le -c:a pcm_f32le $n.ref.f32
  $X/decode $n.ac3 $n.out.f32 2> $n.json
  cmp $n.out.f32 $n.ref.f32 $((acmod + 1)) "$n $(ffprobe -v error -show_entries stream=sample_rate -of csv=p=0 $n.ac3) Hz"
done; done; done
frames() { # ours ours_no_noise ref nch label: per-frame mismatch, explained by noise or not
  ${PYTHON:-python3} -I -c "
import sys, numpy as np
o, q, r = (np.fromfile(sys.argv[i], '<f4').astype(np.float64).reshape(-1, int(sys.argv[4])) for i in (1, 2, 3))
F = len(r) // 1536
fr = lambda x: np.abs(x[:F * 1536]).reshape(F, -1).max(1)
mis = fr(o - r) > 1e-6; noisy = fr(o - q) > 0; noisy |= np.r_[False, noisy[:-1]]
print('| %s | %d | %d | %d | %d | %.1e |' % (sys.argv[5], F, mis.sum(), (mis & noisy).sum(), (mis & ~noisy).sum(),
      fr(o - r)[~noisy].max() if (~noisy).any() else 0))
" $1 $2 $3 $4 $5
}
# 2. FATE / FFmpeg-encoded AC-3 streams, dither stripped, then bsid patched (crc1/crc2 fixed).
# The patched streams misparse (they were allocated for bsid 8), so both decoders conceal; a frame
# mismatch is "noise" when our own noise-on and noise-off outputs differ there (misparsed dithflag).
# Columns: frames, mismatching, of which noise, unexplained, max |diff| over noise-free frames.
for f in ../../agent-ac3/es/*.ac3 ../../agent-ac3/syn/*.ac3; do
  s=$(basename $f .ac3); ch=$(ffprobe -v error -show_entries stream=channels -of csv=p=0 $f | head -1)
  $X/strip_dither $f $s.nd.ac3 > /dev/null 2>&1
  for b in 9 10; do
    python3 -I $R/patch_bsid.py $s.nd.ac3 $s.b$b.ac3 $b 2> /dev/null
    ffmpeg -nostdin -v quiet -y -i $s.b$b.ac3 -f f32le -c:a pcm_f32le $s.b$b.ref.f32 || true
    $X/decode $s.b$b.ac3 $s.b$b.out.f32 2> /dev/null
    $X/decode $s.b$b.ac3 $s.b$b.nn.f32 --no-noise 2> /dev/null
    frames $s.b$b.out.f32 $s.b$b.nn.f32 $s.b$b.ref.f32 $ch "$s bsid $b"
  done
done
