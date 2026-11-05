#!/bin/bash
# macOS (Apple silicon) pinned hardware FFmpeg variants for the evaluation
# (research tooling). Homebrew provides molten-vk, vulkan-loader/headers,
# spirv-headers, shaderc, meson, ninja (versions recorded in the receipt).
#   ffmpeg-mac-hw       FFmpeg 9.0.2: gating software set + avfilter + Vulkan + VideoToolbox
#   ffmpeg-mac-placebo  same + libplacebo v7.351.0 (built here from the pinned tag)
#   hw_capture, hw_capture_placebo
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../../.." && pwd)
BASE=${BASE:-$SRC/.research/capture-d5/hw}
ARCHIVE=${ARCHIVE:-$SRC/.research/capture-d5/src-ffmpeg/ffmpeg-9.0.2.tar.xz}
JOBS=$(sysctl -n hw.ncpu)
mkdir -p "$BASE/logs" "$BASE/src" "$BASE/headers"
echo "8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e  $ARCHIVE" | shasum -a 256 --check
# flip_vulkan is enabled only because FFmpeg 9.0.2 builds vulkan/flip.comp.spv.o
# (needed by vflip_vulkan) under CONFIG_FLIP_VULKAN_FILTER alone.
FILTERS=buffer,buffersink,format,hwupload,hwdownload,scale,scale_vulkan,vflip_vulkan,flip_vulkan,scale_vt
ARGS=(--enable-shared --disable-static --enable-pic "--extra-cflags=-Dstatic_assert=_Static_assert -I/opt/homebrew/include"
  --disable-programs --disable-doc --disable-avdevice --disable-network --disable-swresample --disable-debug
  --disable-everything --disable-autodetect --enable-zlib
  --enable-demuxer=matroska,mov,mpegts,h264,hevc
  --enable-parser=h264,hevc,aac,ac3,flac,dvdsub,dvbsub
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata
  --enable-decoder=pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt
  --enable-decoder=aac,ac3,eac3,dca,mp3,mp2,flac,opus,vorbis,truehd,mlp,alac,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,pcm_s32le,pcm_s32be
  --enable-protocol=file --enable-encoder=mpeg1video --enable-muxer=mpegts
  --enable-avfilter --enable-vulkan --enable-videotoolbox)
build() {
  local name=$1 prefix=$2; shift 2
  [ -f "$prefix/.done" ] && return
  rm -rf "$BASE/src/$name" "$prefix" && mkdir -p "$BASE/src/$name"
  tar -xf "$ARCHIVE" -C "$BASE/src/$name" --strip-components=1
  cd "$BASE/src/$name"
  printf '%s\n' "$@" > "$BASE/logs/$name.configure-args"
  ./configure --prefix="$prefix" "$@" > "$BASE/logs/$name.configure.log" 2>&1
  make -j"$JOBS" > "$BASE/logs/$name.make.log" 2>&1
  make install > "$BASE/logs/$name.install.log" 2>&1
  touch "$prefix/.done"
  cd "$BASE" && rm -rf "${BASE:?}/src/$name"
}
build mac-hw "$BASE/ffmpeg-mac-hw" "${ARGS[@]}" --enable-filter=$FILTERS
cd "$BASE/headers"
[ -d libplacebo ] || git clone -q --depth 1 --branch v7.351.0 --recurse-submodules --shallow-submodules https://code.videolan.org/videolan/libplacebo.git libplacebo
git -C libplacebo rev-parse HEAD > libplacebo.commit
PL="$BASE/libplacebo-mac"
if [ ! -f "$PL/.done" ]; then
  rm -rf "$BASE/src/libplacebo-build"
  # libplacebo 7.351's vk.xml generator fails on Python 3.15's ElementTree;
  # pin the generator interpreter to 3.12 (the Ubuntu 24.04 host's version).
  printf "[binaries]\npython = '/opt/homebrew/bin/python3.12'\npython3 = '/opt/homebrew/bin/python3.12'\n" > "$BASE/logs/libplacebo-native.ini"
  meson setup "$BASE/src/libplacebo-build" "$BASE/headers/libplacebo" --native-file "$BASE/logs/libplacebo-native.ini" \
    --prefix="$PL" --libdir=lib --buildtype=release \
    -Dvulkan=enabled -Dshaderc=enabled -Dglslang=disabled -Dopengl=disabled -Dd3d11=disabled -Dlcms=disabled \
    -Ddemos=false -Dtests=false > "$BASE/logs/libplacebo-mac.setup.log" 2>&1
  ninja -C "$BASE/src/libplacebo-build" install > "$BASE/logs/libplacebo-mac.build.log" 2>&1
  touch "$PL/.done"
  rm -rf "${BASE:?}/src/libplacebo-build"
fi
PKG_CONFIG_PATH="$PL/lib/pkgconfig:/opt/homebrew/lib/pkgconfig" build mac-placebo "$BASE/ffmpeg-mac-placebo" "${ARGS[@]}" \
  --enable-filter=$FILTERS,libplacebo --enable-libplacebo "--extra-ldflags=-Wl,-rpath,$PL/lib"
for v in hw placebo; do
  P="$BASE/ffmpeg-mac-$v"; out="$BASE/hw_capture"; [ $v = placebo ] && out="$BASE/hw_capture_placebo"
  cc -std=c11 -O2 -Wall -Wextra -o "$out" "$SRC/harness/capture/hw/hw_capture.c" -I"$P/include" -L"$P/lib" \
    -Wl,-rpath,"$P/lib" -Wl,-rpath,"$PL/lib" -lavfilter -lavformat -lavcodec -lswscale -lavutil
done
{
  shasum -a 256 "$BASE/hw_capture" "$BASE/hw_capture_placebo"
  echo "libplacebo $(cat "$BASE/headers/libplacebo.commit")"
  brew list --versions molten-vk vulkan-loader vulkan-headers spirv-headers shaderc meson ninja
  cc --version | head -1
} > "$BASE/logs/mac-setup-receipt.txt"
echo "mac hw setup complete"
