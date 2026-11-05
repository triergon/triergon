# Desktop playback sessions

`triergon-sim` accepts AVC/HEVC Annex B access units and AC-3, E-AC-3, ADTS AAC
or DTS core audio. Enable its `codecs` feature for the in-process Rust
decoders. Video publishes timestamped 1920×1080 RGBA pictures. Audio delivers
interleaved stereo s16 PCM at the decoder's output rate. The caller owns the
clock, packet framing, output devices and generation changes.

The [playback crate](../playback/README.md) has a host test that reads an AVC/AAC
MP4, converts the source packets through the existing framing adapters and
feeds both sessions. It verifies two pictures, 2,048 stereo sample frames,
output at the expected clock positions, identical replay after a seek to
start, and teardown. The seek resets AAC perceptual-noise state with the
rest of the decoder's signal history.

The standalone D4 v3 receipt records 417 of 417 comparable outcomes and
delivery timing, peak live heap and total peak RSS passes on all 17 native
Linux ARM64 workloads. It retains the other 37 original dispositions. See
`.research/d4-v3-numerical-activation-plan-v1/D4-V3-SELECTED-QUALIFICATION.json`
at the repository root. That receipt applies to the pinned standalone
workers. The combined workspace and the seek reset have local functional
evidence. Application integration and target verification remain open.
