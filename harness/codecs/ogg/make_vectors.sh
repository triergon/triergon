#!/bin/sh
# Generate the generated Ogg inputs of the reader comparison (cargo xtask codecs readers).
#
# usage: harness/codecs/ogg/make_vectors.sh FFMPEG OUT_DIR
#
# FFMPEG must be FFmpeg 9.0.2 with libopus, which the decode profile of cargo xtask
# prepare-ffmpeg does not build. The Ogg muxer draws random serial numbers, so a new run
# writes different bytes: the compared inputs are the copies archived in the
# audio-readers-mac-arm64 evidence set (inputs/ogg-generated), identified by their
# SHA-256 in harness/codecs/reader-manifest.json. Writes
# Opus, Vorbis and FLAC streams, a multiplexed and a multichannel Opus file, two chained
# files, and three damaged copies of a_opus.opus (harness/codecs/ogg/damage.py).
set -eu
if [ "${1:-}" = "--layouts" ]; then
    [ "$#" -eq 5 ] || { echo "usage: $0 --layouts FFMPEG OGGENC OPUSENC OUT_DIR" >&2; exit 2; }
    shift
    FFMPEG=$1
    OGGENC=$2
    OPUSENC=$3
    OUT=$4
    mkdir -p "$OUT"
    pcm=$(mktemp)
    trap 'rm -f "$pcm"' EXIT HUP INT TERM
    for channels in 1 2 3 4 5 6 7 8; do
        expression=
        channel=0
        while [ "$channel" -lt "$channels" ]; do
            expression="${expression}${expression:+|}0.1*sin(2*PI*$((300 + channel * 79))*t)"
            channel=$((channel + 1))
        done
        "$FFMPEG" -nostdin -v error -y -f lavfi -i "aevalsrc=$expression:s=48000:d=0.22" \
            -c:a pcm_s16le -f s16le "$pcm"
        "$OGGENC" -Q --raw --raw-rate 48000 --raw-chan "$channels" --serial "$((1000 + channels))" \
            -o "$OUT/vorbis-$channels.ogg" "$pcm"
        "$OPUSENC" --quiet --raw --raw-rate 48000 --raw-chan "$channels" --serial "$((2000 + channels))" \
            "$pcm" "$OUT/opus-$channels.opus"
    done
    exit 0
fi
[ $# -eq 2 ] || { echo "usage: $0 FFMPEG OUT_DIR" >&2; exit 2; }
FFMPEG=$1
OUT=$2
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
cd "$OUT"
f() { "$FFMPEG" -nostdin -v error -y "$@"; }
f -f lavfi -i "sine=f=440:d=1.234:r=48000" -ac 2 -c:a libopus -page_duration 100000 a_opus.opus
f -f lavfi -i "sine=f=300:d=0.6:r=48000" -ac 2 -c:a libopus b_opus.opus
f -f lavfi -i "sine=f=440:d=1.234:r=44100" -ac 2 -strict -2 -c:a vorbis a_vorbis.ogg
f -f lavfi -i "sine=f=300:d=0.6:r=44100" -ac 2 -strict -2 -c:a vorbis b_vorbis.ogg
f -f lavfi -i "sine=f=440:d=1.234:r=44100" -c:a flac -f ogg a_flac.oga
f -f lavfi -i "sine=f=440:d=0.9:r=96000" -ac 6 -sample_fmt s32 -c:a flac -f ogg flac_51_24.oga
f -f lavfi -i "sine=f=440:d=0.5:r=48000" -f lavfi -i "sine=f=660:d=0.7:r=48000" -map 0 -map 1 \
    -c:a libopus -page_duration 20000 mux2.opus
f -f lavfi -i "sine=f=500:d=1.5:r=48000" -ac 6 -c:a libopus -mapping_family 1 mc_opus.opus
cat a_opus.opus b_opus.opus > chain_opus.opus
cat a_vorbis.ogg b_vorbis.ogg > chain_vorbis.ogg
python3 -I "$HERE/damage.py" a_opus.opus
