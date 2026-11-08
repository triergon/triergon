#!/bin/sh
# Generate the MPEG audio vectors of the mp3, mp2 and mp1 lanes of cargo xtask codecs grade.
#
# usage: harness/codecs/mp3/make_vectors.sh FFMPEG OUT_DIR
#
# Writes into OUT_DIR (the manifest's .research/mp3/vectors) streams encoded from one
# synthetic signal by libmp3lame, mp2 and mp2fixed, random Layer I/II frames
# (harness/codecs/mp3/random_frames.py) and MPEG-2 intensity stereo frames
# (harness/codecs/mp3/lsf_intensity.py). FFMPEG must be FFmpeg 9.0.2 with libmp3lame,
# which the decode profile of cargo xtask prepare-ffmpeg does not build; the vectors
# are identified by their SHA-256 in harness/codecs/correctness-manifest.json.
set -eu
[ $# -eq 2 ] || { echo "usage: $0 FFMPEG OUT_DIR" >&2; exit 2; }
FFMPEG=$1
OUT=$2
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"

# Six seconds of chirp, tone bursts with sharp onsets (short blocks), noise and a
# different right channel (joint stereo).
SIGNAL="aevalsrc='0.3*sin(2*PI*(200+600*t)*t)+0.5*lt(mod(t,0.37),0.004)*sin(2*PI*1000*t)+0.04*(2*random(0)-1)|0.3*sin(2*PI*440*t)+0.2*sin(2*PI*(5000+2000*sin(t))*t)+0.3*lt(mod(t+0.1,0.53),0.003)':s=48000:d=6"

encode() { # NAME FORMAT ARGS...
    name=$1 format=$2
    shift 2
    "$FFMPEG" -nostdin -v error -y -f lavfi -i "$SIGNAL" "$@" -f "$format" "$OUT/$name"
}
encode l3_44k_128_joint.mp3 mp3 -ar 44100 -c:a libmp3lame -b:a 128k
encode l3_48k_vbr2.mp3 mp3 -ar 48000 -c:a libmp3lame -q:a 2
encode l3_32k_64_mono.mp3 mp3 -ar 32000 -ac 1 -c:a libmp3lame -b:a 64k
encode l3_44k_320_stereo.mp3 mp3 -ar 44100 -c:a libmp3lame -b:a 320k -joint_stereo 0
encode l3_44k_32_joint.mp3 mp3 -ar 44100 -c:a libmp3lame -b:a 32k
encode l3_44k_abr160.mp3 mp3 -ar 44100 -c:a libmp3lame -abr 1 -b:a 160k
encode l3_44k_128_noreservoir.mp3 mp3 -ar 44100 -c:a libmp3lame -b:a 128k -reservoir 0
encode l3_44k_128_id3.mp3 mp3 -ar 44100 -c:a libmp3lame -b:a 128k -id3v2_version 3 \
    -write_id3v1 1 -metadata title=vector -metadata artist=triergon
encode l3_44k_128_noxing.mp3 mp3 -ar 44100 -c:a libmp3lame -b:a 128k -write_xing 0
encode l3_22k_64_joint.mp3 mp3 -ar 22050 -c:a libmp3lame -b:a 64k
encode l3_24k_vbr4.mp3 mp3 -ar 24000 -c:a libmp3lame -q:a 4
encode l3_16k_32_mono.mp3 mp3 -ar 16000 -ac 1 -c:a libmp3lame -b:a 32k
encode l3_11k_32.mp3 mp3 -ar 11025 -c:a libmp3lame -b:a 32k
encode l3_12k_24_mono.mp3 mp3 -ar 12000 -ac 1 -c:a libmp3lame -b:a 24k
encode l3_8k_16.mp3 mp3 -ar 8000 -c:a libmp3lame -b:a 16k
encode l3_8k_8_mono.mp3 mp3 -ar 8000 -ac 1 -c:a libmp3lame -b:a 8k
encode l2_48k_192.mp2 mp2 -ar 48000 -c:a mp2 -b:a 192k
encode l2_48k_384.mp2 mp2 -ar 48000 -c:a mp2 -b:a 384k
encode l2_44k_128.mp2 mp2 -ar 44100 -c:a mp2 -b:a 128k
encode l2_44k_80_mono.mp2 mp2 -ar 44100 -ac 1 -c:a mp2 -b:a 80k
encode l2_44k_32_mono.mp2 mp2 -ar 44100 -ac 1 -c:a mp2 -b:a 32k
encode l2_32k_48_mono.mp2 mp2 -ar 32000 -ac 1 -c:a mp2 -b:a 48k
encode l2_32k_192.mp2 mp2 -ar 32000 -c:a mp2 -b:a 192k
encode l2_24k_64.mp2 mp2 -ar 24000 -c:a mp2 -b:a 64k
encode l2_22k_96.mp2 mp2 -ar 22050 -c:a mp2 -b:a 96k
encode l2_16k_32_mono.mp2 mp2 -ar 16000 -ac 1 -c:a mp2 -b:a 32k
encode l2fixed_48k_256.mp2 mp2 -ar 48000 -c:a mp2fixed -b:a 256k
encode l2fixed_44k_64_mono.mp2 mp2 -ar 44100 -ac 1 -c:a mp2fixed -b:a 64k
python3 -I "$HERE/random_frames.py" "$OUT"
python3 -I "$HERE/lsf_intensity.py" "$OUT"
