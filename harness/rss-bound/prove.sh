#!/bin/sh
# Build the kernel RSS bound module for the running kernel, attest a fresh
# registration and write the proof that `cargo xtask build-workers` and
# `cargo xtask report --memory` accept with TRIERGON_RSS_KERNEL_PROOF.
#
#   usage: harness/rss-bound/prove.sh [--review REVIEW] OUT [USERSPACE]
#
# Run from the repository root as the measurement user, with passwordless sudo
# for insmod, rmmod, chown and reading debugfs, after `cargo xtask build-workers`
# has built the observer and probe. The review of this module source for this
# kernel release is the committed one, harness/rss-bound/reviews/SOURCE-RELEASE.json
# (the first 12 hexadecimal digits of the module source's SHA-256, and `uname -r`);
# --review names another. Give it, and OUT, relative to the repository root. The
# module stays loaded and owned by the measurement user on success; unload it with
# `sudo rmmod triergon_rss_bound` before any timing run.
#
# The module is built and loaded where the kernel's headers and module tools
# are. For a 32-bit user space on a 64-bit kernel, run this on the host from the
# repository inside that user space and name its root as USERSPACE: the check is
# then built and run, and the architecture and page size recorded, inside it
# under `setarch linux32`, through the module's 32-bit ioctl entry point.
set -eu
sha() { sha256sum "$1" | cut -d' ' -f1; }
release=$(uname -r)
review=
if [ "${1:-}" = --review ]; then
    review=$2
    shift 2
fi
out=$1
userspace=${2:-}
build=target/rss-bound
module=$build/triergon_rss_bound.ko
if [ -z "$review" ]; then
    review=harness/rss-bound/reviews/$(sha harness/rss-bound/triergon_rss_bound.c | cut -c1-12)-$release.json
    [ -f "$review" ] || { echo "no review of this module source for kernel $release: $review" >&2; exit 1; }
fi

# Run a command in the measurement user space, as the measurement user or as root.
if [ -n "$userspace" ]; then
    inside=${PWD#"$(cd "$userspace" && pwd)"}
    [ "$inside" != "$PWD" ] || { echo "the repository is not inside $userspace" >&2; exit 1; }
    measure() { sudo -n chroot "$userspace" setarch linux32 sudo -n -u "#$(id -u)" sh -c "cd '$inside' && $*"; }
    measure_root() { sudo -n chroot "$userspace" setarch linux32 sh -c "cd '$inside' && $*"; }
else
    measure() { sh -c "$*"; }
    measure_root() { sudo -n sh -c "$*"; }
fi

# The review's own fields, as preflight reads them: a prior source or kernel the
# review names elsewhere does not count.
python3 - "$review" "$(sha harness/rss-bound/triergon_rss_bound.c)" "$release" <<'EOF' || exit 1
import json
import sys

review, source, release = sys.argv[1:]
document = json.load(open(review))
for key, wanted, covers in (
    ("module_source_sha256", source, "this module source"),
    ("kernel_release", release, "kernel " + release),
    ("status", "reviewed-conditional-upper-bound", "a reviewed upper bound"),
):
    if document.get(key) != wanted:
        sys.exit(f"review does not cover {covers}: {review}")
EOF
for binary in target/harness/allocations.so target/harness/memory-check; do
    [ -f "$binary" ] || { echo "$binary missing: run cargo xtask build-workers" >&2; exit 1; }
done
[ ! -e /sys/module/triergon_rss_bound ] || { echo "module already loaded" >&2; exit 1; }

# The module and the check include the API as rss_bound.h; both copy the one
# tracked header, so module_api and observer_api are the same file.
rm -rf "$build"
mkdir -p "$build" "$out"
cp harness/rss-bound/triergon_rss_bound.c harness/rss-bound/Makefile "$build/"
cp harness/rss-bound-api.h "$build/rss_bound.h"
make -s -C "/lib/modules/$release/build" M="$PWD/$build" modules
measure "cc -O2 -Wall -Wextra -Werror -I $build harness/rss-bound/check.c -o $build/kernel-check"

# Negative attestation: a control write while armed must invalidate the session.
sudo -n /sbin/insmod "$module"
sudo -n cat /sys/kernel/debug/kprobes/enabled > "$out/negative-armed.stdout"
[ "$(cat "$out/negative-armed.stdout")" = 1 ]
status=0
measure_root "$build/kernel-check invalidate" > "$out/negative-invalidate.stdout" || status=$?
[ "$status" = 1 ]
grep -q '"missed":0,"errors":8,' "$out/negative-invalidate.stdout"
sudo -n /sbin/rmmod triergon_rss_bound

# Fresh registration for the measurements, owned by the measurement user.
sudo -n /sbin/insmod "$module"
sudo -n chown "$(id -u):$(id -g)" /dev/triergon_rss_bound
sudo -n cat /sys/kernel/debug/kprobes/enabled > "$out/fresh-armed.stdout"
[ "$(cat "$out/fresh-armed.stdout")" = 1 ]
measure "$build/kernel-check" > "$out/positive.stdout"
# Concurrent sessions: four processes held open side by side, each bound on its own.
pids=""
for n in 1 2 3; do
    measure "$build/kernel-check hold" > "$out/concurrent-$n.stdout" &
    pids="$pids $!"
done
measure "$build/kernel-check hold" > "$out/concurrent-4.stdout"
for pid in $pids; do wait "$pid"; done
for n in 1 2 3 4; do grep -q '"missed":0,"errors":0,' "$out/concurrent-$n.stdout"; done
cat "$out"/concurrent-[1-4].stdout > "$out/concurrent.stdout"

pin() { printf '"%s": {"path": "%s", "sha256": "%s"}' "$1" "$2" "$(sha "$2")"; }
note=/sys/module/triergon_rss_bound/notes/.note.gnu.build-id
{
    printf '{\n  "schema": "linux-rss-kernel-bound-proof-v1",\n'
    printf '  "status": "reviewed-conditional-upper-bound",\n  "abi": 1,\n'
    printf '  "kernel_release": "%s",\n  "architecture": "%s",\n  "page_size": %s,\n' \
        "$release" "$(measure "uname -m")" "$(measure "getconf PAGESIZE")"
    printf '  "files": {\n'
    printf '    %s,\n' "$(pin module_source harness/rss-bound/triergon_rss_bound.c)"
    printf '    %s,\n' "$(pin module_api harness/rss-bound-api.h)"
    printf '    %s,\n' "$(pin module_binary "$module")"
    printf '    %s,\n' "$(pin observer_source harness/allocations-linux.c)"
    printf '    %s,\n' "$(pin observer_api harness/rss-bound-api.h)"
    printf '    %s,\n' "$(pin probe_source harness/portable-memory-check.c)"
    printf '    %s,\n' "$(pin observer_binary target/harness/allocations.so)"
    printf '    %s\n  },\n' "$(pin probe_binary target/harness/memory-check)"
    printf '  "runtime_files": {\n'
    printf '    "/sys/kernel/notes": "%s",\n' "$(sha /sys/kernel/notes)"
    printf '    "/proc/sys/kernel/random/boot_id": "%s",\n' "$(sha /proc/sys/kernel/random/boot_id)"
    printf '    "%s": "%s"\n  },\n' "$note" "$(sha "$note")"
    printf '  "coverage": {\n    "complete_hooks": true,\n'
    printf '    "single_thread_since_constructor": true,\n'
    printf '    "full_thp_disabled_before_open": true,\n'
    printf '    "lifecycle": "constructor-open-to-END-ioctl",\n'
    printf '    "review_artifact": {"path": "%s", "sha256": "%s"}\n  },\n' "$review" "$(sha "$review")"
    printf '  "deployment": {"armed_sha256": "%s", "positive_sha256": "%s", "concurrent_sha256": "%s", "negative_sha256": "%s"},\n' \
        "$(sha "$out/fresh-armed.stdout")" "$(sha "$out/positive.stdout")" "$(sha "$out/concurrent.stdout")" "$(sha "$out/negative-invalidate.stdout")"
    printf '  "numeric_qualification": false\n}\n'
} > "$out/RSS-PROOF.json"
echo "$out/RSS-PROOF.json"
