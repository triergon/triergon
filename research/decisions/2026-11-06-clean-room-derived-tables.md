# Clean-room tables derived from reference output

On 2026-11-06 the owner settled the provenance question raised in
[the DCA status](../d4/dca/STATUS.md#table-provenance-owner-review-requested): values
recovered black-box from the reference FFmpeg binary's observed output are **not**
copying FFmpeg, provided no FFmpeg source was read. Such values may ship under
Triergon's own licence.

## Scope

The decision covers the tables and behavioural rules that earlier lanes measured
this way:

- the DTS ADPCM (D.10.1) and high-frequency VQ (D.10.2) codebooks in
  `crates/codec-dca/src/vq.rs`. ETSI TS 102 114 defines both as normative but
  does not print them;
- the FFmpeg-effective perfect-reconstruction prototype in
  `crates/codec-dca/src/pr_table.rs`;
- rules recovered the same way elsewhere, for example the simulator scaling rules
  in `crates/sim/src/scale.rs`.

## Conditions

- The recovery must stay reproducible from tracked inputs. For the DCA tables,
  `research/d4/dca/regenerate_tables.sh` builds the probe streams from
  `examples/synth`, decodes them with the pinned FFmpeg 9.0.2 binary and runs
  `recover_vq.py`, `recover_pr.py` and `gen_vq.py`. On 2026-11-06 both generated
  files were byte-identical to the committed ones at `origin/main` `4f06ca3`.
- The FFmpeg binary is used only as a black-box oracle: its inputs and outputs, never
  its source.
- Hand edits to generated tables are not allowed. Regenerate them and record the
  result.

## Not covered

Patent and trademark exposure for shipping a DTS decoder. That exposure is unchanged
from Streem's existing FFmpeg use and needs its own review if required.

The "Provenance needs owner review" comments in the two generated files predate this
decision. They will be updated the next time the tables are regenerated with a changed
generator, because editing them now would change frozen source identities.
