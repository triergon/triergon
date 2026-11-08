#!/bin/bash
# Generate the FLAC, MPEG audio, Vorbis and Opus timing workloads of
# harness/codecs/file-workloads.json into OUT (default .research/codec-inputs).
#
# usage: harness/codecs/file-workloads.sh FFMPEG [OUT]
#
# Sources are deterministic lavfi signals (two detuned, slowly modulated tones per
# channel over seeded pink noise) rendered by FFMPEG, the pinned decode reference;
# the encoders are the reference implementations: flac 1.4.3, LAME 3.100, oggenc
# 1.4.2 (libvorbis), opusenc 0.2 (libopus 1.4), and FFMPEG's own mp2 encoder.
set -euo pipefail
FFMPEG=$1
OUT=${2:-.research/codec-inputs}
mkdir -p "$OUT"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# source NAME RATE CHANNELS SECONDS: a 24-bit WAV.
source() {
    local name=$1 rate=$2 channels=$3 seconds=$4 tones="" mix=""
    for ((c = 0; c < channels; c++)); do
        local f=$((180 + 37 * c)) g=$((271 + 53 * c))
        tones+="${tones:+|}0.22*sin(2*PI*$f*t)*(1+0.6*sin(2*PI*0.$((3 + c))*t))+0.14*sin(2*PI*$g.5*t)"
    done
    "$FFMPEG" -nostdin -v error -y \
        -f lavfi -i "aevalsrc='$tones':s=$rate:d=$seconds" \
        -f lavfi -i "anoisesrc=color=pink:seed=$((7 + channels)):amplitude=0.06:r=$rate:d=$seconds" \
        -filter_complex "[1]aformat=channel_layouts=mono,asplit=$channels$(for ((c = 0; c < channels; c++)); do printf '[n%d]' $c; done);$(
            printf '[0]channelsplit=channel_layout=%s' "$(layout "$channels")"
            for ((c = 0; c < channels; c++)); do printf '[t%d]' $c; done
            printf ';'
            for ((c = 0; c < channels; c++)); do printf '[t%d][n%d]amix=inputs=2:normalize=0[m%d];' $c $c $c; done
            for ((c = 0; c < channels; c++)); do printf '[m%d]' $c; done
            printf 'join=inputs=%d:channel_layout=%s' "$channels" "$(layout "$channels")"
        )" \
        -c:a pcm_s24le "$TMP/$name.wav"
}

layout() {
    case $1 in 1) echo mono ;; 2) echo stereo ;; 6) echo 5.1 ;; esac
}

source cd 44100 2 30
source hd 96000 6 10
source film 48000 6 10
source music 48000 2 30
source speech 16000 1 30

"$FFMPEG" -nostdin -v error -y -i "$TMP/cd.wav" -c:a pcm_s16le "$TMP/cd16.wav"
flac -s -f -5 -o "$OUT/flac-16-44k-stereo.flac" "$TMP/cd16.wav"
flac -s -f -8 -o "$OUT/flac-24-96k-5.1.flac" "$TMP/hd.wav"
lame -S --noreplaygain -b 128 -m j "$TMP/cd16.wav" "$OUT/mp3-cbr-128-joint.mp3"
lame -S --noreplaygain -V 2 "$TMP/cd16.wav" "$OUT/mp3-vbr-v2.mp3"
"$FFMPEG" -nostdin -v error -y -i "$TMP/music.wav" -c:a mp2 -b:a 256k -f mp2 "$OUT/mp2-256-48k.mp2"
oggenc -Q -q 5 --serial 1 -o "$OUT/vorbis-q5-stereo.ogg" "$TMP/cd16.wav"
oggenc -Q -q 3 --serial 2 -o "$OUT/vorbis-q3-5.1.ogg" "$TMP/film.wav"
opusenc --quiet --serial 1 --bitrate 128 "$TMP/music.wav" "$OUT/opus-music-128-stereo.opus"
opusenc --quiet --serial 2 --speech --bitrate 24 "$TMP/speech.wav" "$OUT/opus-speech-24-mono.opus"
opusenc --quiet --serial 3 --bitrate 256 "$TMP/film.wav" "$OUT/opus-5.1-256.opus"
sha256sum "$OUT"/{flac,mp3,mp2,vorbis,opus}-*
