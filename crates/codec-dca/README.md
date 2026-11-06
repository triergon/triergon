# triergon-codec-dca

DTS Coherent Acoustics core decoder (ETSI TS 102 114), including the XCh and X96
extensions carried inside the core frame. It is a codec-layer crate whose only dependency
is [triergon-codec](../codec/README.md) (`AudioDecoder`, `AudioFrame`, `ChannelLayout`,
`Speaker`). It decodes one core frame at a time to planar `f32` in FFmpeg's native
channel order. It does not decode DTS-HD extension substreams (XBR, XLL, LBR, or XXCh/X96
in an extension substream); the caller strips those.

## Usage

```rust
use triergon_codec_dca::{AudioFrame, DecodeError, Decoder};

fn decode_frames<'a>(frames: impl IntoIterator<Item = &'a [u8]>) -> Result<(), DecodeError> {
    let mut dec = Decoder::new();
    let mut frame = AudioFrame::default();
    for core_frame in frames {               // one complete core frame per call
        dec.decode(core_frame, &mut frame)?; // replaces `frame`'s contents
        // frame.sample_rate, frame.layout.speakers(), frame.planes[i]
    }
    dec.reset(); // after a seek: drops ADPCM, LFE and synthesis history
    Ok(())
}
```

`examples/decode_dts.rs IN.dts [OUT.f32]` splits a stream by sync and FSIZE, skips extension
substreams and writes interleaved f32le.

## Supported

| Area | Coverage |
|---|---|
| Packing | 16-bit and 14-bit words, big and little endian (`Packing::detect`) |
| Channels | AMODE 0–9 with or without LFE; XCh adds one back-centre channel (6.1) |
| Rates | All core sample rates; X96 doubles the output rate |
| Frames | 8–128 blocks, 1–16 subframes, 1–4 subsubframes |
| Coding | Huffman and block-code quantizers, all ABITS and scale-factor tables, transient mode, ADPCM prediction, high-frequency VQ, joint intensity, sum/difference |
| Synthesis | 32-band QMF (both filter banks), 64-band QMF for X96, LFE 64x and 128x interpolation |
| Extensions | XCh (one channel), X96 revisions 1–8 |

## Behaviour and limits

- Output layout: AMODE 0 → FC; 1–4 → FL FR; 5 → 3.0; 6 → 3.0(back); 7 → 4.0;
  8 → quad(side); 9 → 5.0(side). The LFF field adds LFE; XCh adds BC. Planes follow the
  triergon-codec [channel order](../codec/README.md#channel-order), e.g. 5.1(side) is FL FR FC LFE SL SR and 6.1 is
  FL FR FC LFE BC SL SR. `layout_name(&layout)` gives FFmpeg's layout name.
- 14-bit packings are normalized to 16-bit big endian; FSIZE counts bytes of the 16-bit
  equivalent stream, as in FFmpeg.
- Errors are typed: `Unsupported(&str)`, `InvalidData(&str)`, `ResourceLimit(&str)`.
  Refused: AMODE 10–15, deficit samples (SHORT ≠ 31), XXCh inside the core frame, XCh with
  more than one channel. A DSYNC mismatch is `InvalidData`.
- A failed frame is dropped without touching any history, so the next frame continues
  as if it were absent.
- X96 with REVNO 0 or above 8 is ignored and the core is decoded at the core rate.
- Not applied, as in FFmpeg: dynamic range (RANGE), dialog normalization. TIMES, AUX and
  OCRC are skipped; PSC is read and ignored.
- Buffers are sized to the stream; there is no steady-state allocation.

### Behaviour taken from FFmpeg rather than the text

- Synthesis gain: 0.25·2^7.5 for the 32-band bank and 0.25·2^8.5 for the 64-band bank,
  with PCM/2^23 as full scale (C.3.6 leaves the scale open).
- Perfect-reconstruction bank (FILTS = 1) uses the prototype FFmpeg effectively applies,
  which differs from Annex D.8 by up to 5.9e-6.
- X96 doubles the LFE with a two-tap interpolator (0.75/0.25), not the Table 6-11 filter.
- Dequantized subband samples, ADPCM output, VQ samples, joint copies, decimated LFE and
  X96 residuals saturate to 24 bits.
- In 8-block frames the LFE history is refilled by repeating the frame's samples.
- ADPCM and LFE history is dropped when a frame needs more subband samples than any frame
  since the last reset.

## Tables

`src/tables.rs` is generated from the ETSI TS 102 114 V1.6.1 text by
`harness/codecs/dca/gen_tables.py`, which also corrects two printed errata (Table D129
prints level 37 twice; Table D65 drops levels ±31/±32 at a page break and is taken from
V1.3.1).

The Annex D.10 VQ codebooks (D.10.1 ADPCM, 4096×4; D.10.2 high-frequency VQ, 1024×32)
are normative but not printed in any edition. They, and the perfect-reconstruction
prototype in `src/pr_table.rs`, are recovered black-box from the reference decoder's
output on synthetic probe streams; no decoder source was read. Running
`harness/codecs/dca/regenerate_tables.sh` with FFmpeg 9.0.2 on `PATH` rebuilds `src/vq.rs`
and `src/pr_table.rs` and checks that they are byte-identical to the committed files.

## Known gaps

- X96 bands with ABITS96 = 0 decode as silence; FFmpeg fills them with noise from an
  unidentified generator.
- X96 residuals at scale index 61/62 with extreme overload differ from FFmpeg.
- Termination frames (FTYPE = 0) decode here; FFmpeg's parser merges them with the next
  frame.
- No coverage: X96 and XCh in one frame, sum/difference with X96.

## Conformance

- Specification: ETSI TS 102 114 V1.6.1 (Table D65 from V1.3.1).
- Reference: FFmpeg 9.0.2's float decoder on the core of every core-carrying FATE `dts/`
  vector (`-bsf:a dca_core`) and on self-authored synthetic streams for features the
  vectors do not reach. Every sample is compared from sample 0: exact rate, layout and
  count; per channel RMS ≤ 8.8e-6 and max |diff| ≤ 6.1035e-5
  (`harness/codecs/numeric-rules.json`).
- `cargo test -p triergon-codec-dca` checks the synthetic scenarios against stored FFmpeg
  fingerprints (`tests/data/synthetic.fp`), packing equivalence, layouts, typed errors and
  corruption without panics; `tests/vectors.rs` checks the FATE vectors when present.
  Fingerprints are regenerated by `harness/codecs/dca/make_fingerprints.sh`.
- Derivation notes: [harness/codecs/dca/STATUS.md](../../harness/codecs/dca/STATUS.md).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).
