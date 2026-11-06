#!/bin/bash
# Regenerate crates/codec-dca/tests/data/{vectors,synthetic}.fp from the FFmpeg oracle
# (development only; requires ffmpeg/ffprobe and run_all.sh's extracted vectors).
set -eu
R=$(cd "$(dirname "$0")/../../.." && pwd)
T=$R/target/agent-dca
FP=$R/research/d4/dca/fingerprint.py
ch_of() { ffprobe -v error -f dts -show_frames -read_intervals "%+#1" -show_entries frame=channels -of csv=p=0 "$1" | head -1 | cut -d, -f1; }
rate_of() { ffprobe -v error -f dts -show_entries stream=sample_rate -of csv=p=0 "$1" | head -1; }
OUT=$R/crates/codec-dca/tests/data/vectors.fp
echo "# FFmpeg 9.0.2 (Homebrew, macOS) float output of core-only bytes (ffmpeg -bsf:a dca_core -f dts); research/d4/dca/fingerprint.py" > $OUT
for f in $T/vec/*.ref.f32; do
  b=$(basename $f .ref.f32); [ $b = xll_51_24_48_none ] && continue
  python3 -I $FP $b $f $(ch_of $T/vec/$b.core.dts) $(rate_of $T/vec/$b.core.dts) 4096 16 >> $OUT
done
SC="basic perfect lfe128 nolfe huff huff_aspf block bigabits blocks128 blocks256s lfe128_short lfe_mixed
size_changes_adpcm adpcm adpcm_h0 adpcm_ovf hfvq hfvq_short hfvq_loud joint joint_huff joint_jlin sumf sums
sumf_stereo scalebooks scalehigh dynf cpf cpf_dynf timef_auxf lossless psc vernum6 vernum8 es_flag amode0
amode1 amode2 amode3 amode4 amode5 amode6 amode7 amode8 amode9 sfreq1 sfreq8 pack_le16 pack_be14 pack_le14
x96_basic x96_lin x96_hi x96_hi_lin x96_rev8 x96_vq x96_adpcm x96_joint x96_aspf x96_lfe128 x96_s58 x96_s60
x96_rev9"
OUT=$R/crates/codec-dca/tests/data/synthetic.fp
echo "# FFmpeg 9.0.2 (Homebrew, macOS) float output of tests/support/scenarios.rs streams; research/d4/dca/fingerprint.py" > $OUT
S=$T/synth; mkdir -p $S
for s in $SC; do
  $T/release/examples/synth $s $S/$s.dts
  ffmpeg -v error -y -f dts -i $S/$s.dts -f f32le -c:a pcm_f32le $S/$s.ref.f32
  python3 -I $FP $s $S/$s.ref.f32 $(ch_of $S/$s.dts) $(rate_of $S/$s.dts) 4096 8 >> $OUT
done
ls -la $R/crates/codec-dca/tests/data/
