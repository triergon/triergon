#!/bin/bash
# Regenerate crates/codec-dca/src/{vq.rs,pr_table.rs} from the clean-room recovery chain and
# compare them byte for byte with the committed tables.
#
#   examples/synth {adpcm,hfvq,pr}_probe -> FFmpeg 9.0.2 binary (black-box oracle, f32 output)
#   -> recover_vq.py / recover_pr.py -> gen_vq.py -> generated sources
#
# No FFmpeg source is read. Requires the pinned FFmpeg 9.0.2 `ffmpeg` on PATH and Python
# with numpy (research tooling only, not a production dependency).
# SPEC_V161_TXT is `pdftotext -layout` of ETSI TS 102 114 V1.6.1 (see gen_tables.py); the
# Annex D.8 prototype in it is the baseline pr_table.rs records its deviation from.
#   usage: harness/codecs/dca/regenerate_tables.sh SPEC_V161_TXT [PYTHON]
# Exit status 0 means both generated files are identical to the committed ones.
set -euo pipefail
R=$(cd "$(dirname "$0")/../../.." && pwd)
SPEC=${1:?usage: regenerate_tables.sh SPEC_V161_TXT [PYTHON]}
PY=${2:-python3}
T=${CARGO_TARGET_DIR:-$R/target}
W=$T/dca-regenerate
mkdir -p "$W"
ffmpeg -version | head -1 | grep -q '^ffmpeg version 9\.0\.2 ' || { echo "need FFmpeg 9.0.2 on PATH" >&2; exit 2; }
CARGO_INCREMENTAL=0 cargo build --quiet --release -p triergon-codec-dca --example synth --manifest-path "$R/Cargo.toml"
SYNTH=$T/release/examples/synth
for probe in adpcm_probe hfvq_probe pr_probe; do
  "$SYNTH" "$probe" "$W/$probe.dts"
  ffmpeg -v error -y -f dts -i "$W/$probe.dts" -f f32le -c:a pcm_f32le "$W/$probe.ref.f32"
done
"$PY" -I "$R/harness/codecs/dca/recover_vq.py" "$W/adpcm_probe.dts.meta" "$W/adpcm_probe.ref.f32" "$W/adpcm.txt"
"$PY" -I "$R/harness/codecs/dca/recover_vq.py" "$W/hfvq_probe.dts.meta" "$W/hfvq_probe.ref.f32" "$W/hfvq.txt"
"$PY" -I "$R/harness/codecs/dca/gen_vq.py" "$W/adpcm.txt" "$W/hfvq.txt" "$W/vq.rs"
"$PY" -I "$R/harness/codecs/dca/recover_pr.py" "$W/pr_probe.dts.meta" "$W/pr_probe.ref.f32" \
  "$SPEC" "$W/pr_table.rs"
status=0
for f in vq.rs pr_table.rs; do
  if cmp -s "$W/$f" "$R/crates/codec-dca/src/$f"; then
    echo "identical  crates/codec-dca/src/$f"
  else
    echo "DIFFERS    crates/codec-dca/src/$f (generated: $W/$f)"; status=1
  fi
done
exit $status
