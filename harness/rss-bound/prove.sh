#!/bin/sh
# Build the kernel RSS bound module for the running kernel, attest a fresh
# registration and write the proof that `cargo xtask build-workers` and
# `cargo xtask report --memory` accept with SAFE_MEDIA_RSS_KERNEL_PROOF.
#
#   usage: harness/rss-bound/prove.sh REVIEW OUT
#
# Run from the repository root as the measurement user, with passwordless sudo
# for insmod, rmmod, chown and reading debugfs, after `cargo xtask build-workers`
# has built the observer and probe. REVIEW is the source review of this module
# source for this kernel release. The module stays loaded and owned by the
# measurement user on success; unload it with `sudo rmmod safe_media_rss_bound`
# before any timing run.
set -eu
review=$1
out=$2
build=target/rss-bound
module=$build/safe_media_rss_bound.ko
sha() { sha256sum "$1" | cut -d' ' -f1; }
release=$(uname -r)

grep -q "\"module_source_sha256\": \"$(sha harness/rss-bound/safe_media_rss_bound.c)\"" "$review" ||
    { echo "review does not cover this module source" >&2; exit 1; }
grep -q "\"kernel_release\": \"$release\"" "$review" ||
    { echo "review does not cover kernel $release" >&2; exit 1; }
grep -q '"status": "reviewed-conditional-upper-bound"' "$review" ||
    { echo "review is not a reviewed upper bound" >&2; exit 1; }
for binary in target/harness/allocations.so target/harness/memory-check; do
    [ -f "$binary" ] || { echo "$binary missing: run cargo xtask build-workers" >&2; exit 1; }
done
[ ! -e /sys/module/safe_media_rss_bound ] || { echo "module already loaded" >&2; exit 1; }

# The module and the check include the API as rss_bound.h; both copy the one
# tracked header, so module_api and observer_api are the same file.
rm -rf "$build"
mkdir -p "$build" "$out"
cp harness/rss-bound/safe_media_rss_bound.c harness/rss-bound/Makefile "$build/"
cp harness/rss-bound-api.h "$build/rss_bound.h"
make -s -C "/lib/modules/$release/build" M="$PWD/$build" modules
cc -O2 -Wall -Wextra -Werror -I "$build" harness/rss-bound/check.c -o "$build/kernel-check"

# Negative attestation: a control write while armed must invalidate the session.
sudo -n /sbin/insmod "$module"
sudo -n cat /sys/kernel/debug/kprobes/enabled > "$out/negative-armed.stdout"
[ "$(cat "$out/negative-armed.stdout")" = 1 ]
status=0
sudo -n "$build/kernel-check" invalidate > "$out/negative-invalidate.stdout" || status=$?
[ "$status" = 1 ]
grep -q '"missed":0,"errors":8,' "$out/negative-invalidate.stdout"
sudo -n /sbin/rmmod safe_media_rss_bound

# Fresh registration for the measurements, owned by the measurement user.
sudo -n /sbin/insmod "$module"
sudo -n chown "$(id -u):$(id -g)" /dev/safe_media_rss_bound
sudo -n cat /sys/kernel/debug/kprobes/enabled > "$out/fresh-armed.stdout"
[ "$(cat "$out/fresh-armed.stdout")" = 1 ]
"$build/kernel-check" > "$out/positive.stdout"

pin() { printf '"%s": {"path": "%s", "sha256": "%s"}' "$1" "$2" "$(sha "$2")"; }
note=/sys/module/safe_media_rss_bound/notes/.note.gnu.build-id
{
    printf '{\n  "schema": "linux-rss-kernel-bound-proof-v1",\n'
    printf '  "status": "reviewed-conditional-upper-bound",\n  "abi": 1,\n'
    printf '  "kernel_release": "%s",\n  "architecture": "%s",\n  "page_size": %s,\n' \
        "$release" "$(uname -m)" "$(getconf PAGESIZE)"
    printf '  "files": {\n'
    printf '    %s,\n' "$(pin module_source harness/rss-bound/safe_media_rss_bound.c)"
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
    printf '  "deployment": {"armed_sha256": "%s", "positive_sha256": "%s", "negative_sha256": "%s"},\n' \
        "$(sha "$out/fresh-armed.stdout")" "$(sha "$out/positive.stdout")" "$(sha "$out/negative-invalidate.stdout")"
    printf '  "numeric_qualification": false\n}\n'
} > "$out/RSS-PROOF.json"
echo "$out/RSS-PROOF.json"
