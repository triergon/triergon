#!/bin/zsh
# Reproduce the harness/codecs/ac3/STATUS.md vector table (development oracle only).
# Run from the worktree root after building examples into target/agent-ac3:
#   CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/agent-ac3 cargo +nightly build --release -p triergon-codec-ac3 --examples
# Inputs: elementary streams in target/agent-ac3/es (FATE .ac3/.eac3 copied, the .ts/.wav
# AC-3 tracks extracted with `ffmpeg -c copy -f ac3`). Media never enters git.
set -e
# PYTHON must have numpy (on this Mac: PYTHON=python3.14).
cd target/agent-ac3
mkdir -p out ref nd
for f in es/*; do
  b=$(basename $f); ext=${b##*.}
  ch=$(ffprobe -v error -show_entries stream=channels -of csv=p=0 $f | head -1)
  lay=$(ffprobe -v error -show_entries stream=channel_layout -of csv=p=0 $f | head -1)
  ffmpeg -v quiet -y -f $ext -c:a $ext -i $f -f f32le -c:a pcm_f32le ref/$b.f32 || true
  ffmpeg -v quiet -y -f $ext -c:a $ext -cons_noisegen 1 -i $f -f f32le -c:a pcm_f32le ref/$b.cng.f32 || true
  ./release/examples/decode $f out/$b.f32 2> out/$b.json
  ./release/examples/strip_dither $f nd/$b 2>/dev/null
  ffmpeg -v quiet -y -f $ext -c:a $ext -i nd/$b -f f32le -c:a pcm_f32le nd/$b.ref.f32 || true
  ./release/examples/decode nd/$b nd/$b.out.f32 2>/dev/null
  ${PYTHON:-python3} -I ../../harness/codecs/ac3/cmp_f32.py "$b" "$lay" $ch out/$b.f32 ref/$b.f32 ref/$b.cng.f32 nd/$b.out.f32 nd/$b.ref.f32 out/$b.json
done
