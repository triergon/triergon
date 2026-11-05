#!/bin/bash
# Linux GPU host setup for the hardware-acceleration evaluation (research tooling).
# Prerequisite (done interactively, recorded in research/capture/hw/README.md):
# NVIDIA 580-server driver + prebuilt kernel modules, nouveau unbound, libegl1.
# Builds under $BASE (default ~/capture-hw):
#   headers/   Khronos Vulkan-Headers v1.4.312, SPIRV-Headers vulkan-sdk-1.4.313.0,
#              nv-codec-headers n13.0.19.0 (pinned tags, commit SHAs recorded)
#   ffmpeg/hw      FFmpeg 9.0.2, gating software component set + avfilter + Vulkan
#   ffmpeg/oracle  default full configure (decoder oracle only)
#   bin/           hw_capture, nvenc_probe, heapcount.so, capture-rust-phases, capture-worker
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../../.." && pwd)
BASE=${BASE:-$HOME/capture-hw}
JOBS=$(nproc)
mkdir -p "$BASE"/{dl,src,ffmpeg,bin,logs,headers}
cd "$BASE/dl"
[ -f ffmpeg-9.0.2.tar.xz ] || curl --fail -L --retry 3 -o ffmpeg-9.0.2.tar.xz https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz
echo "8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e  ffmpeg-9.0.2.tar.xz" | sha256sum --check
cd "$BASE/headers"
clone() { [ -d "$2" ] || git clone -q --depth 1 --branch "$3" "$1" "$2"; git -C "$2" rev-parse HEAD > "$2.commit"; }
clone https://github.com/KhronosGroup/Vulkan-Headers.git Vulkan-Headers v1.4.312
clone https://github.com/KhronosGroup/SPIRV-Headers.git SPIRV-Headers vulkan-sdk-1.4.313.0
clone https://github.com/FFmpeg/nv-codec-headers.git nv-codec-headers n13.0.19.0
INC="-I$BASE/headers/Vulkan-Headers/include -I$BASE/headers/SPIRV-Headers/include -I$BASE/headers/nv-codec-headers/include"
HW_ARGS=(--enable-shared --disable-static --enable-pic "--extra-cflags=-Dstatic_assert=_Static_assert $INC"
  --disable-programs --disable-doc --disable-avdevice --disable-network --disable-swresample --disable-debug
  --disable-everything --disable-autodetect --enable-zlib
  --enable-demuxer=matroska,mov,mpegts,h264,hevc
  --enable-parser=h264,hevc,aac,ac3,flac,dvdsub,dvbsub
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata
  --enable-decoder=pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt
  --enable-decoder=aac,ac3,eac3,dca,mp3,mp2,flac,opus,vorbis,truehd,mlp,alac,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,pcm_s32le,pcm_s32be
  --enable-protocol=file --enable-encoder=mpeg1video --enable-muxer=mpegts
  # flip_vulkan: 9.0.2 builds vulkan/flip.comp.spv.o (needed by vflip_vulkan) under CONFIG_FLIP_VULKAN_FILTER only.
  --enable-avfilter --enable-filter=buffer,buffersink,format,hwupload,hwdownload,scale,scale_vulkan,vflip_vulkan,flip_vulkan
  --enable-vulkan --enable-libdrm)
build() {
  local name=$1; shift
  local prefix="$BASE/ffmpeg/$name"
  [ -f "$prefix/.done" ] && return
  rm -rf "$BASE/src/$name" && mkdir -p "$BASE/src/$name"
  tar -xf "$BASE/dl/ffmpeg-9.0.2.tar.xz" -C "$BASE/src/$name" --strip-components=1
  cd "$BASE/src/$name"
  printf '%s\n' "$@" > "$BASE/logs/$name.configure-args"
  ./configure --prefix="$prefix" "$@" > "$BASE/logs/$name.configure.log" 2>&1
  make -j"$JOBS" > "$BASE/logs/$name.make.log" 2>&1
  make install > "$BASE/logs/$name.install.log" 2>&1
  touch "$prefix/.done"
}
build hw "${HW_ARGS[@]}"
build oracle --disable-doc --disable-debug
P="$BASE/ffmpeg/hw"
cc -std=c11 -O2 -Wall -Wextra -o "$BASE/bin/hw_capture" "$SRC/harness/capture/hw/hw_capture.c" -I"$P/include" \
  -L"$P/lib" -Wl,-rpath,"$P/lib" -lavfilter -lavformat -lavcodec -lswscale -lavutil
cc -O2 -I"$BASE/headers/nv-codec-headers/include" -o "$BASE/bin/nvenc_probe" "$SRC/harness/capture/hw/nvenc_probe.c" -ldl
cc -O2 -fPIC -shared -o "$BASE/bin/heapcount.so" "$SRC/harness/capture/hw/heapcount.c" -ldl -pthread
. "$HOME/.cargo/env"
(cd "$SRC/harness/capture/hw/rust-phases" && CARGO_INCREMENTAL=0 cargo +nightly build --release > "$BASE/logs/rust-phases.log" 2>&1)
cp "$SRC/harness/capture/hw/rust-phases/target/release/capture-rust-phases" "$BASE/bin/"
(cd "$SRC" && CARGO_INCREMENTAL=0 cargo +nightly build --release --locked -p triergon-capture > "$BASE/logs/cargo.log" 2>&1)
python3 - "$BASE" <<'PY'
import hashlib, json, os, subprocess, sys
base = sys.argv[1]
sha = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()
run = lambda *a: subprocess.run(a, capture_output=True, text=True).stdout.strip()
receipt = {
    "ffmpeg_archive_sha256": sha(f"{base}/dl/ffmpeg-9.0.2.tar.xz"),
    "headers": {h: open(f"{base}/headers/{h}.commit").read().strip() for h in ("Vulkan-Headers", "SPIRV-Headers", "nv-codec-headers")},
    "hw_configure_args": open(f"{base}/logs/hw.configure-args").read().splitlines(),
    "binaries": {n: sha(f"{base}/bin/{n}") for n in os.listdir(f"{base}/bin")},
    "cc": run("cc", "--version").splitlines()[0], "rustc": run("rustc", "+nightly", "-V"),
    "glslang": run("glslangValidator", "--version").splitlines()[0],
    "nvidia_smi": run("nvidia-smi", "--query-gpu=name,driver_version,vbios_version", "--format=csv,noheader"),
    "vulkaninfo": [l.strip() for l in run("vulkaninfo", "--summary").splitlines() if any(k in l for k in ("deviceName", "driverInfo", "apiVersion"))],
    "lscpu": run("lscpu"),
}
json.dump(receipt, open(f"{base}/setup-receipt.json", "w"), indent=1)
print("gpu setup complete")
PY
