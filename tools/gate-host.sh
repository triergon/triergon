#!/bin/bash
# A gate host: an AArch64 Linux machine (Graviton2 for 32-bit ARMv7) that runs the
# gate natively, or in the armhf user space at /srv/armhf. Run as the measurement
# user, who has passwordless sudo.
#
#   tools/gate-host.sh setup aarch64|armv7 REVISION
#       packages, kernel headers and Rust on the host; for armv7 the armhf user
#       space from evidence set armv7-userland ($USERLAND_URL, an address of its
#       archive), mounted at every boot, with its own Rust; the repository at
#       REVISION in the measurement user's home, in the user space for armv7; and
#       the gate's media inputs: each archive in $INPUTS (space-separated paths or
#       URLs) unpacked at the repository root
#   tools/gate-host.sh prepare
#       every pinned reference build (restored from .research/reference-cache
#       where a pack of its key is there), the generated fixtures, vectors, font
#       corpus and manifests, and the workers
#   tools/gate-host.sh checkout REVISION
#       the repository at REVISION (fetched from origin, or from the bundle
#       $BUNDLE), with the keys of the reference builds it needs
#   tools/gate-host.sh ready I/N REVIEW
#       the prepare step, then the preflight of shard I of N with the RSS module
#       review REVIEW (a path in the repository) and one proof into a scratch
#       directory: the host can measure
#   tools/gate-host.sh run I/N REVIEW OUT [--full]
#       shard I of N into OUT (in the repository): timing alone on CPU 1, the
#       kernel proof, the workers rebuilt with it, its preflight, memory on CPUs 1
#       to the last and the grades with the module loaded; then the module is
#       unloaded
#
# Each command appends its steps to ~/gate-host.log.
set -euo pipefail

USERSPACE=/srv/armhf
LOG=$HOME/gate-host.log
REPOSITORY=${TRIERGON_REPOSITORY:-https://github.com/triergon/triergon.git}
ARM32=thumbv7neon-unknown-linux-gnueabihf
USERLAND_SHA256=f93de19128a5bc101c125a4a1f7c61f42062675186cfc86f627d220d1d079533
NIGHTLY=nightly-2026-10-17
PACKAGES="build-essential pkg-config zlib1g-dev python3 python3-numpy make git zstd curl
    ca-certificates jq time binutils autoconf automake libtool autopoint gettext
    linux-headers-$(uname -r) linux-tools-common linux-tools-$(uname -r)"
USERSPACE_PACKAGES="autoconf automake libtool autopoint gettext"

log() { echo "$(date -u +%FT%TZ) $*" >> "$LOG"; }
armv7() { [ -d "$USERSPACE/home/ubuntu" ]; }
# The repository as the host sees it.
repository() { if armv7; then echo "$USERSPACE$HOME/triergon"; else echo "$HOME/triergon"; fi; }
# Run a shell command in the measurement environment, in the repository.
inside() {
    local command="cd ~/triergon 2>/dev/null || cd ~; . ~/.cargo/env; export CARGO_INCREMENTAL=0; $*"
    if armv7; then
        sudo chroot "$USERSPACE" setarch linux32 sudo -u "$USER" -H bash -lc "$command"
    else
        bash -lc "$command"
    fi
}
step() { log "$1"; shift; if "$@"; then log "done"; else local s=$?; log "FAILED ($s)"; return $s; fi; }
fetch() { case $1 in http*) curl -fsS --retry 3 "$1";; *) cat "$1";; esac; }

rust() { # TARGETS...: rustup with the pinned toolchain, the targets and the pinned nightly
    inside "command -v rustup >/dev/null || curl -fsS https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none >/dev/null"
    inside "rustup toolchain install \$(sed -n 's/^channel *= *\"\\(.*\\)\"/\\1/p' rust-toolchain.toml) --profile minimal -c rustfmt -c clippy $* >/dev/null"
    inside "rustup toolchain install $NIGHTLY --profile minimal $* >/dev/null"
}

setup() {
    local target=$1 revision=$2
    # Nothing may install or upgrade packages, or change the kernel, while a gate runs.
    sudo systemctl disable --now unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer >/dev/null 2>&1 || true
    sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq
    # shellcheck disable=SC2086
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $PACKAGES >/dev/null
    sudo apt-mark hold "linux-image-$(uname -r)" "linux-headers-$(uname -r)" >/dev/null
    if [ "$target" = armv7 ]; then
        fetch "$USERLAND_URL" > /tmp/userland.tar.gz
        echo "$USERLAND_SHA256  /tmp/userland.tar.gz" | sha256sum --check --quiet
        sudo mkdir -p "$USERSPACE"
        sudo tar -xpzf /tmp/userland.tar.gz --numeric-owner -C "$USERSPACE"
        rm /tmp/userland.tar.gz
        sudo tee /etc/systemd/system/armhf-userspace.service >/dev/null <<EOF
[Unit]
Description=Mounts of the armhf user space
After=local-fs.target sys-kernel-debug.mount
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'for d in proc sys dev dev/pts sys/kernel/debug; do mkdir -p $USERSPACE/\$d; mountpoint -q $USERSPACE/\$d || mount --bind /\$d $USERSPACE/\$d; done; cp /etc/resolv.conf $USERSPACE/etc/resolv.conf'
[Install]
WantedBy=multi-user.target
EOF
        sudo systemctl daemon-reload
        sudo systemctl enable --now armhf-userspace.service
        sudo install -d -o "$(id -u)" -g "$(id -g)" "$USERSPACE$HOME"
        sudo chroot "$USERSPACE" setarch linux32 sh -c \
            "DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $USERSPACE_PACKAGES >/dev/null"
    fi
    inside "git clone -q $REPOSITORY ~/triergon && cd ~/triergon && git checkout -q --detach $revision"
    if armv7; then rust --target "$ARM32"; else rust; fi
    for archive in ${INPUTS:-}; do fetch "$archive" | tar -xzf - -C "$(repository)"; done
}

prepare() {
    local profile
    for profile in matroska formats video subtitles decode capture capture-small; do
        step "prepare-ffmpeg $profile" inside "cargo xtask prepare-ffmpeg --jobs 8 --profile $profile"
    done
    for command in "fonts prepare --jobs 8" "shape prepare --jobs 8" "ass prepare --jobs 8" \
        fixtures fetch-vectors "fonts fetch" "fonts manifest" "shape manifest" "raster manifest" \
        "ass manifest" "framing fixtures" build-workers; do
        step "$command" inside "cargo xtask $command"
    done
    log PREPARED
}

checkout() {
    local revision=$1
    if [ -n "${BUNDLE:-}" ]; then
        cp "$BUNDLE" "$(repository)/../source.bundle"
        inside "git fetch -q ~/source.bundle $revision"
    else
        inside "git fetch -q origin $revision"
    fi
    inside "git checkout -q --detach FETCH_HEAD && git rev-parse HEAD && cargo xtask references keys"
}

prove() { # OUT: build, load and prove the module, leaving it loaded
    local review=$1 out=$2
    [ ! -e /sys/module/triergon_rss_bound ] || sudo rmmod triergon_rss_bound
    if armv7; then
        (cd "$(repository)" && harness/rss-bound/prove.sh "$review" "$out" "$USERSPACE")
    else
        (cd "$(repository)" && harness/rss-bound/prove.sh "$review" "$out")
    fi
}

ready() {
    local shard=$1 review=$2
    prepare
    step preflight inside "cargo xtask gate preflight --shard $shard --review $review"
    step prove-scratch prove "$review" .research/rss-bound-scratch
    sudo rmmod triergon_rss_bound
    log READY
}

run() {
    local shard=$1 review=$2 out=$3 full=${4:-}
    local proof=.research/rss-bound
    [ ! -e /sys/module/triergon_rss_bound ] || sudo rmmod triergon_rss_bound
    step "timing $shard" inside "cargo xtask gate run --stage timing --shard $shard --output $out $full" || true
    step prove prove "$review" "$proof"
    step build-workers-kernel inside "TRIERGON_RSS_KERNEL_PROOF=$proof/RSS-PROOF.json cargo xtask build-workers"
    step preflight-kernel inside "cargo xtask gate preflight --shard $shard --review $review --kernel"
    step "memory $shard" inside "cargo xtask gate run --stage memory --shard $shard --output $out $full" || true
    sudo rmmod triergon_rss_bound
    log DONE
}

command=${1:?usage: tools/gate-host.sh setup|prepare|checkout|ready|run ...}
shift
case $command in
setup) step "setup $*" setup "$@"; log "SETUP DONE" ;;
prepare) prepare ;;
checkout) checkout "$@" ;;
ready) ready "$@" ;;
run) run "$@" ;;
*) echo "unknown command $command" >&2; exit 2 ;;
esac
