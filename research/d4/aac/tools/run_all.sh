#!/bin/sh
# Decode every ADTS vector in $VEC (default target/agent-aac/vec) with the crate example built
# in $TGT (default target/agent-aac) and compare with the FFmpeg reference (vec/ref/*.f32,
# made by `ffmpeg -f aac -i X -f f32le -c:a pcm_f32le`). Optional $1: shell glob on the name.
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
TGT=${TGT:-$ROOT/target/agent-aac}
VEC=${VEC:-$ROOT/target/agent-aac/vec}
mkdir -p "$TGT/out"
for f in "$VEC"/*.adts; do
  n=$(basename "$f" .adts)
  [ -n "$1" ] && case "$n" in $1) ;; *) continue;; esac
  info=$("$TGT/release/examples/decode" "$f" "$TGT/out/$n.f32" 2>&1 | tail -1)
  ch=$(/opt/homebrew/bin/ffprobe -v error -f aac -i "$f" -show_entries stream=channels -of csv=p=0 | head -1)
  rl=$(/opt/homebrew/bin/ffprobe -v error -f aac -i "$f" -show_entries stream=sample_rate,channel_layout -of csv=p=0 | head -1)
  cmp=$("$TGT/release/examples/compare" "$TGT/out/$n.f32" "$VEC/ref/$n.f32" "$ch" 2>&1)
  echo "$n | ref $rl | $info | $cmp"
done
