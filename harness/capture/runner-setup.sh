#!/bin/bash
# Native Linux setup for the capture comparison (tooling).
# Builds, under $BASE (default ~/capture-reference):
#   ffmpeg/small    FFmpeg 9.0.2 with the application's development component set,
#                   including --enable-small
#   ffmpeg/speed    the same component set without --enable-small: the gating
#                   ("best allowed configuration") runtime reference
#   ffmpeg/oracle   default full configure with ffmpeg/ffprobe: independent
#                   decoder/demuxer oracle, never a timed reference
#   bin/reference-capture-{speed,small}  harness/reference.c, capture build
#   bin/allocations.so   shared observer harness/allocations-linux.c
#   bin/capture-worker   candidate (cargo +nightly --release --locked)
# and records every hash/argv in $BASE/setup-receipt.json. The qualification run
# then uses them:
#   cargo xtask capture qualify --candidate $BASE/bin/capture-worker \
#     --reference $BASE/bin/reference-capture-speed \
#     --reference-small $BASE/bin/reference-capture-small \
#     --oracle-bin $BASE/ffmpeg/oracle/bin --observer $BASE/bin/allocations.so \
#     --inputs DIR --out DIR
# with TRIERGON_PYTHON naming a Python with numpy for the input generator.
set -euo pipefail
SRC=$(cd "$(dirname "$0")/../.." && pwd)
BASE=${BASE:-$HOME/capture-reference}
JOBS=${JOBS:-$(nproc)}
VERSION=9.0.2
SHA=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
mkdir -p "$BASE"/{dl,src,ffmpeg,bin,logs}
cd "$BASE/dl"
[ -f ffmpeg-$VERSION.tar.xz ] || curl --fail --location --retry 3 -o ffmpeg-$VERSION.tar.xz https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz
echo "$SHA  ffmpeg-$VERSION.tar.xz" | sha256sum --check

SMALL_ARGS=(--enable-shared --disable-static --enable-pic
  "--extra-cflags=-Dstatic_assert=_Static_assert"
  --disable-programs --disable-doc --disable-avdevice --disable-avfilter
  --disable-network --disable-swresample
  --disable-debug --enable-small
  --disable-everything --disable-autodetect --enable-zlib
  --enable-demuxer=matroska,mov,mpegts,h264,hevc
  --enable-parser=h264,hevc,aac,ac3,flac,dvdsub,dvbsub
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata
  --enable-decoder=pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt
  --enable-decoder=aac,ac3,eac3,dca,mp3,mp2,flac,opus,vorbis,truehd,mlp,alac,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,pcm_s32le,pcm_s32be
  --enable-protocol=file
  --enable-encoder=mpeg1video --enable-muxer=mpegts)

build() {
  local name=$1; shift
  local prefix="$BASE/ffmpeg/$name"
  if [ -f "$prefix/.done" ]; then return; fi
  rm -rf "$BASE/src/$name" && mkdir -p "$BASE/src/$name"
  tar -xf "$BASE/dl/ffmpeg-$VERSION.tar.xz" -C "$BASE/src/$name" --strip-components=1
  cd "$BASE/src/$name"
  printf '%s\n' "$@" > "$BASE/logs/$name.configure-args"
  ./configure --prefix="$prefix" "$@" > "$BASE/logs/$name.configure.log" 2>&1
  make -j"$JOBS" > "$BASE/logs/$name.make.log" 2>&1
  make install > "$BASE/logs/$name.install.log" 2>&1
  touch "$prefix/.done"
}

build small "${SMALL_ARGS[@]}"
SPEED_ARGS=()
for a in "${SMALL_ARGS[@]}"; do [ "$a" = --enable-small ] || SPEED_ARGS+=("$a"); done
build speed "${SPEED_ARGS[@]}"
build oracle --disable-doc --disable-debug

for name in speed small; do
  P="$BASE/ffmpeg/$name"
  cc -std=c11 -O2 -Wall -Wextra -Werror -DTRIERGON_CAPTURE_REFERENCE -o "$BASE/bin/reference-capture-$name" \
    "$SRC/harness/reference.c" -I"$P/include" -L"$P/lib" \
    -Wl,-rpath,"$P/lib" -lavformat -lavcodec -lswscale -lavutil
done
cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared -fno-builtin -Wl,-z,now \
  "$SRC/harness/allocations-linux.c" -I"$SRC/harness" -ldl -pthread -o "$BASE/bin/allocations.so"

cd "$SRC"
. "$HOME/.cargo/env"
CARGO_INCREMENTAL=0 cargo +nightly build --release --locked -p capture-worker > "$BASE/logs/cargo-build.log" 2>&1
cp target/release/capture-worker "$BASE/bin/capture-worker"

python3 - "$BASE" "$SRC" <<'PY'
import hashlib, json, os, platform, subprocess, sys
base, src = sys.argv[1], sys.argv[2]
def sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()
def run(*a):
    return subprocess.run(a, capture_output=True, text=True).stdout.strip()
receipt = {
    "ffmpeg_archive_sha256": sha(f"{base}/dl/ffmpeg-9.0.2.tar.xz"),
    "configure_args": {n: open(f"{base}/logs/{n}.configure-args").read().splitlines() for n in ("small", "speed")},
    "oracle_configure_args": open(f"{base}/logs/oracle.configure-args").read().splitlines(),
    "binaries": {n: sha(f"{base}/bin/{n}") for n in os.listdir(f"{base}/bin")},
    "libraries": {f"{n}/{l}": sha(f"{base}/ffmpeg/{n}/lib/{l}") for n in ("speed", "small")
                  for l in sorted(os.listdir(f"{base}/ffmpeg/{n}/lib")) if ".so." in l and not os.path.islink(f"{base}/ffmpeg/{n}/lib/{l}")},
    "cc": run("cc", "--version").splitlines()[0],
    "rustc": run("rustc", "+nightly", "-Vv"),
    "cargo_lock_sha256": sha(f"{src}/Cargo.lock"),
    "uname": platform.uname()._asdict(),
    "cpu": run("lscpu"),
    "glibc": run("ldd", "--version").splitlines()[0],
}
json.dump(receipt, open(f"{base}/setup-receipt.json", "w"), indent=1)
print("setup complete")
PY
