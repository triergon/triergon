#!/bin/sh
# Decode every vector with examples/decode_mp3 and grade it against FFmpeg's float
# decoder output (the gate for Layer III) and its default decoder output; then decode
# a few vectors with a reset at frame 50 against FFmpeg flushing there.
#
# usage: harness/codecs/mp3/grade.sh   (after harness/codecs/mp3/make_vectors.sh)
set -eu
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
OUT="$ROOT/.research/mp3"
mkdir -p "$OUT/out"
cd "$ROOT"
CARGO_INCREMENTAL=0 cargo build --release -q -p triergon-codec-mp3 --example decode_mp3
for input in "$OUT"/vectors/* .research/fate-samples/mp3-conformance/*.bit; do
    name=$(basename "$input")
    name=${name%.*}
    [ -f "$OUT/ref/$name.f32" ] || { echo "{\"name\": \"$name\", \"status\": \"NO-REFERENCE\"}"; continue; }
    ./target/release/examples/decode_mp3 "$input" "$OUT/out/$name.f32" 2>"$OUT/out/$name.json"
    float=$(python3 -I harness/codecs/mp3/compare.py "$OUT/ref/$name.f32" "$OUT/out/$name.f32" "$name")
    default=$(python3 -I harness/codecs/mp3/compare.py "$OUT/ref/$name.default.f32" "$OUT/out/$name.f32" "$name")
    echo "float   $float"
    cmp -s "$OUT/ref/$name.f32" "$OUT/ref/$name.default.f32" || echo "default $default"
done
for name in l3_44k_128_joint l3_22k_64_joint l3_48k_vbr2 l3_8k_8_mono l2_44k_128 l1_48k_256_joint; do
    input=$(ls "$OUT"/vectors/$name.*)
    "$OUT/bin/reference" --flush-at 50 "$input" "$OUT/ref/$name.flush50.f32" 2>/dev/null
    ./target/release/examples/decode_mp3 "$input" "$OUT/out/$name.reset50.f32" --reset-at 50 2>/dev/null
    echo "reset   $(python3 -I harness/codecs/mp3/compare.py "$OUT/ref/$name.flush50.f32" "$OUT/out/$name.reset50.f32" "$name@50")"
done
