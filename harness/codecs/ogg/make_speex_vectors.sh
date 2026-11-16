#!/bin/sh
# Generate the Speex inputs of the reader comparison (cargo xtask codecs readers).
#
# usage: harness/codecs/ogg/make_speex_vectors.sh FFMPEG SPEEXENC OUT_DIR
#
# FFMPEG writes the sine inputs as WAV (any FFmpeg 9.0.2 build); SPEEXENC is speexenc
# 1.2.1 (the Debian and Ubuntu `speex` package). speexenc draws random serial numbers,
# so a new run writes different bytes: the compared inputs are the archived copies,
# identified by their SHA-256 in harness/codecs/reader-manifest.json. Writes
# narrowband, wideband (three frames a packet, VBR), ultra-wideband stereo and
# Skeleton-multiplexed Speex streams, and a chained file of two narrowband streams.
set -eu
[ $# -eq 3 ] || { echo "usage: $0 FFMPEG SPEEXENC OUT_DIR" >&2; exit 2; }
FFMPEG=$1
SPEEXENC=$2
OUT=$3
mkdir -p "$OUT"
cd "$OUT"
wav() { "$FFMPEG" -nostdin -v error -y -f lavfi -i "sine=f=$1:d=$2:r=$3" -ac "$4" -c:a pcm_s16le "$5"; }
wav 440 1.234 8000 1 nb.wav
wav 300 0.6 16000 1 wb.wav
wav 300 0.6 8000 1 nb2.wav
wav 500 0.9 32000 2 uwb.wav
"$SPEEXENC" -n nb.wav speex_nb.spx
"$SPEEXENC" -w --nframes 3 --vbr wb.wav speex_wb.spx
"$SPEEXENC" -u uwb.wav speex_uwb_stereo.spx
"$SPEEXENC" -w --skeleton wb.wav speex_skeleton.spx
"$SPEEXENC" -n nb2.wav nb2.spx
cat speex_nb.spx nb2.spx > chain_speex.spx
rm nb.wav wb.wav uwb.wav nb2.wav nb2.spx
