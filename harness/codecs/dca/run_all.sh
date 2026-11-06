#!/bin/bash
# Development oracle sweep (macOS, non-evidence): decodes every selected vector and every
# synthetic scenario with examples/decode and compares against FFmpeg's float output of the
# same core-only bytes. Scratch lives in target/agent-dca/.
#   usage: harness/codecs/dca/run_all.sh [vectors|synth|all]
set -u
R=$(cd "$(dirname "$0")/../../.." && pwd)
T=$R/target/agent-dca
B=$T/release/examples
FATE=$R/.research/fate-samples/dts
mode=${1:-all}
cmp() { # ref out ch
  python3 -I "$R/harness/codecs/dca/compare.py" "$1" "$2" "$3"
}
row() { # name file
  local name=$1 f=$2 ref=$3
  local info j ch rate lay frate flay fch
  info=$(ffprobe -v error -f dts -show_frames -select_streams a -read_intervals "%+#1" -show_entries frame=channels,channel_layout -of compact "$f" 2>/dev/null | head -1)
  fch=$(echo "$info" | sed -E 's/.*channels=([0-9]+).*/\1/'); flay=$(echo "$info" | sed -E 's/.*channel_layout=([^|]*).*/\1/')
  frate=$(ffprobe -v error -f dts -show_entries stream=sample_rate -of csv=p=0 "$f" 2>/dev/null | head -1)
  j=$($B/decode "$f" "$f.out.f32" 2>&1 | tail -1)
  ch=$(echo "$j" | sed -E 's/.*"channels":([0-9]+).*/\1/')
  rate=$(echo "$j" | sed -E 's/.*"sample_rate":([0-9]+).*/\1/')
  if [ ! -s "$ref" ]; then
    printf "%-26s ffmpeg-refused  ours: %s\n" "$name" "$(echo "$j" | sed -E 's/.*"frames":([0-9]+).*"errors":([0-9]+).*/frames=\1 errors=\2/')"
    return
  fi
  local c; c=$(cmp "$ref" "$f.out.f32" "$ch")
  local meta="rate=$rate/$frate ch=$ch/$fch"
  [ "$rate" = "$frate" ] && [ "$ch" = "$fch" ] || meta="$meta MISMATCH"
  printf "%-26s %-34s %s\n" "$name" "$meta" "$c"
}
if [ "$mode" != synth ]; then
  V=$T/vec; mkdir -p $V
  for f in "$FATE"/*.dts "$FATE"/dts.ts "$FATE"/dcadec-suite/*.dtshd; do
    [ -e "$f" ] || { echo "vector missing: $f"; continue; }
    b=$(basename "$f"); b=${b%.*}
    [ "$b" = xll_51_24_48_none ] && continue  # no core substream
    [ -s $V/$b.core.dts ] || ffmpeg -v error -y -i "$f" -map 0:a:0 -c:a copy -bsf:a dca_core -f dts $V/$b.core.dts
    [ -s $V/$b.ref.f32 ] || ffmpeg -v error -y -f dts -i $V/$b.core.dts -f f32le -c:a pcm_f32le $V/$b.ref.f32
    row "$b" $V/$b.core.dts $V/$b.ref.f32
  done
fi
if [ "$mode" != vectors ]; then
  S=$T/synth; mkdir -p $S
  for s in basic perfect lfe128 nolfe huff huff_aspf block bigabits blocks128 blocks256s lfe128_short lfe_mixed \
           size_changes_adpcm adpcm adpcm_h0 adpcm_ovf hfvq hfvq_short hfvq_loud joint joint_huff joint_jlin \
           sumf sums sumf_stereo scalebooks scalehigh dynf cpf cpf_dynf timef_auxf lossless psc term31 short5 \
           vernum6 vernum8 es_flag amode0 amode1 amode2 amode3 amode4 amode5 amode6 amode7 amode8 amode9 amode10 \
           sfreq1 sfreq2 sfreq3 sfreq6 sfreq7 sfreq8 sfreq11 sfreq12 pack_le16 pack_be14 pack_le14 imp_f1_b3 \
           x96_basic x96_lin x96_hi x96_hi_lin x96_rev8 x96_vq x96_adpcm x96_joint x96_aspf x96_lfe128 \
           x96_s58 x96_s60 x96_zero x96_s62; do
    $B/synth $s $S/$s.dts || continue
    ffmpeg -v error -y -f dts -i $S/$s.dts -f f32le -c:a pcm_f32le $S/$s.ref.f32 2>/dev/null
    row "$s" $S/$s.dts $S/$s.ref.f32
  done
fi
