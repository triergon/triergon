#!/bin/bash
# Second pinned FFmpeg hardware variant for the evaluation (research tooling):
# FFmpeg 9.0.2 + libplacebo v7.351.0 (vf_libplacebo: RGBA -> YUV 4:2:0 fully on
# the GPU). Run after gpu-setup.sh; reuses its pinned headers and archive.
#   ffmpeg/hw-placebo  hw variant + --enable-libplacebo + libplacebo filter
#   bin/hw_capture_placebo
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../../.." && pwd)
BASE=${BASE:-$HOME/capture-hw}
JOBS=$(nproc)
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q meson ninja-build python3-jinja2 libshaderc-dev libvulkan-dev pkg-config > "$BASE/logs/placebo-apt.log" 2>&1
dpkg-query -W meson ninja-build libshaderc-dev libvulkan-dev > "$BASE/logs/placebo-apt.versions"
cd "$BASE/headers"
[ -d libplacebo ] || git clone -q --depth 1 --branch v7.351.0 --recurse-submodules --shallow-submodules https://code.videolan.org/videolan/libplacebo.git libplacebo
git -C libplacebo rev-parse HEAD > libplacebo.commit
PL="$BASE/ffmpeg/libplacebo"
if [ ! -f "$PL/.done" ]; then
  rm -rf "$BASE/src/libplacebo-build"
  meson setup "$BASE/src/libplacebo-build" "$BASE/headers/libplacebo" --prefix="$PL" --libdir=lib --buildtype=release \
    -Dvulkan=enabled -Dshaderc=enabled -Dglslang=disabled -Dopengl=disabled -Dd3d11=disabled -Dlcms=disabled \
    -Ddemos=false -Dtests=false > "$BASE/logs/libplacebo.setup.log" 2>&1
  ninja -C "$BASE/src/libplacebo-build" install > "$BASE/logs/libplacebo.build.log" 2>&1
  touch "$PL/.done"
fi
mapfile -t ARGS < "$BASE/logs/hw.configure-args"
ARGS=("${ARGS[@]/--enable-filter=buffer,buffersink,format,hwupload,hwdownload,scale,scale_vulkan,vflip_vulkan,flip_vulkan/--enable-filter=buffer,buffersink,format,hwupload,hwdownload,scale,scale_vulkan,vflip_vulkan,flip_vulkan,libplacebo}")
ARGS+=(--enable-libplacebo "--extra-ldflags=-Wl,-rpath,$PL/lib")
P="$BASE/ffmpeg/hw-placebo"
if [ ! -f "$P/.done" ]; then
  rm -rf "$BASE/src/hw-placebo" && mkdir -p "$BASE/src/hw-placebo"
  tar -xf "$BASE/dl/ffmpeg-9.0.2.tar.xz" -C "$BASE/src/hw-placebo" --strip-components=1
  cd "$BASE/src/hw-placebo"
  printf '%s\n' "${ARGS[@]}" > "$BASE/logs/hw-placebo.configure-args"
  PKG_CONFIG_PATH="$PL/lib/pkgconfig" ./configure --prefix="$P" "${ARGS[@]/--prefix=*/}" > "$BASE/logs/hw-placebo.configure.log" 2>&1
  make -j"$JOBS" > "$BASE/logs/hw-placebo.make.log" 2>&1
  make install > "$BASE/logs/hw-placebo.install.log" 2>&1
  touch "$P/.done"
fi
cc -std=c11 -O2 -Wall -Wextra -o "$BASE/bin/hw_capture_placebo" "$SRC/harness/capture/hw/hw_capture.c" -I"$P/include" \
  -L"$P/lib" -Wl,-rpath,"$P/lib" -Wl,-rpath,"$PL/lib" -lavfilter -lavformat -lavcodec -lswscale -lavutil
sha256sum "$BASE/bin/hw_capture_placebo" > "$BASE/logs/hw_capture_placebo.sha256"
echo "placebo setup complete"
