# triergon-codec-aac

MPEG-4 AAC decoder for ADTS frames and raw AAC-LC access units. The ADTS decoder
supports Main, LC and LTP, HE-AAC (SBR) and HE-AAC v2 (Parametric Stereo). It depends on
[triergon-bitstream](../bitstream/README.md) and
[triergon-codec](../codec/README.md), whose `AudioDecoder` trait it implements and
whose `AudioFrame`, `ChannelLayout` and `Speaker` it re-exports. The caller supplies
complete access units and receives planar `f32` in the order described by `ChannelLayout`.

## Usage

```rust
use triergon_codec_aac::{AudioFrame, Decoder, adts};

// Two silent mono AAC-LC frames at 48 kHz; any ADTS stream works the same way.
let silent = [0xff, 0xf1, 0x4c, 0x40, 0x01, 0x7f, 0xfc, 0x00, 0xc8, 0x00, 0x07];
let stream = [silent, silent].concat();

let mut dec = Decoder::new();
let mut frame = AudioFrame::default();
let mut pos = 0;
while let Some((at, header)) = adts::find_frame(&stream, pos) {
    let au = &stream[at..at + header.frame_length];
    dec.decode(au, &mut frame)?; // replaces `frame`'s contents
    // frame.sample_rate, frame.layout.speakers(), frame.planes[i]
    assert_eq!((frame.sample_rate, frame.samples_per_channel), (48_000, 1024));
    pos = at + header.frame_length;
}
dec.reset(); // after a seek: drops overlap, prediction, PNS, SBR and PS history
# Ok::<(), triergon_codec_aac::DecodeError>(())
```

`examples/decode_aac.rs IN.aac OUT.f32` writes interleaved f32le and prints a JSON summary.

`RawLcDecoder` decodes one complete raw data block of at most 64 KiB to 1024 samples
per channel, configured from a checked AudioSpecificConfig. `new_in` takes the indexed
core rate and fixed channel configuration 1–7, 11, 12 or 14, whose audible elements
follow Table 1.19, including the distinct 7.1 Front, Back and Top layouts
([ISO/IEC 14496-3:2009/Amd.4](https://standards.iteh.ai/catalog/standards/iso/c0071f4c-5db7-4cf4-a32e-6b75c874d46d/iso-iec-14496-3-2009-amd-4-2013)).
`with_layout_in` with `compat::layout::Policy::ConventionalSurround` gives FFmpeg's
default mapping instead: a back pair in configurations 5, 6, 11 and 14, and
configuration 7 as 7.1 Back. `with_program_config_in` takes the byte-aligned PCE of
channel configuration zero: up to eight channels with distinct speaker positions, coded
elements matched by type and tag, height information applied when its extension is
complete and its CRC-8 valid, and `Feature::RawProgramLayout` for an ambiguous mapping
([ISO/IEC 14496-3:2001/Cor.2:2004, 4.5.1](https://standards.iteh.ai/catalog/standards/iso/09c0e8dc-4b21-438e-a1f1-665e44bfb859/iso-iec-14496-3-2001-cor-2-2004)).
That program stays in effect when a packet carries its own PCE. The configuration must
use 1024-sample frames; a packet carrying SBR is refused (`Feature::RawSbr`). The raw
decoder synthesizes directly into the caller's reserved planes and reuses them.

## Features

| Feature | Default | Effect |
|---|---|---|
| `simd` | off | `std::simd` forms of the DCT-IV passes after the input gather (pre-twiddle, radix-4 stages, post-twiddle and the last radix-2 stage fused with it, four points at a time), which serve the SBR QMF banks and the IMDCT; of the SBR QMF analysis and synthesis windowing and shuffles; of the covariance sums of HF generation (four source bands at a time); and of the Parametric Stereo hybrid filters (four slots at a time). Requires a nightly toolchain (`portable_simd`); `cargo xtask check-simd` uses the pinned one. |

Without `simd` the crate builds on stable and uses the scalar code. Each vector lane
performs the scalar operations in the same order, so output is identical where the vector
unit follows IEEE 754 (AArch64); ARMv7 NEON flushes subnormals to zero. LLVM emits no NEON
float code from the scalar loops on `thumbv7neon`, so the gate build builds the AAC
decode path (`codec-aac-worker` and the AAC decode example) with the pinned nightly and
this feature on both targets; every other worker stays on stable.

## Supported

| Area | Coverage |
|---|---|
| Transport | ADTS, CRC-protected or not, 1–4 raw data blocks per frame (CRC parsed, not checked); single raw AAC-LC blocks |
| Object types | Main (with prediction), LC, LTP |
| Elements | SCE, CPE (common window, M/S), LFE, CCE (dependent and independent coupling), DSE, FIL, PCE incl. height extension |
| Tools | Huffman section/scalefactor/spectral coding, pulse data, TNS, intensity stereo, PNS, sine/KBD windows, all window sequences |
| SBR | Implicit signalling; output at twice the core rate from the first SBR frame; all grid classes, coupled and balanced envelopes, up to 6 patches, limiter, sinusoids |
| PS | Implicit signalling (mono stream becomes stereo); 20- and 34-band hybrid, IID/ICC/IPD/OPD, switching between 20 and 34 bands |
| Rates | Sampling-frequency indices 0–12 |
| Layouts | ADTS configurations 1–7 and PCE layouts up to 93 channels; raw fixed configurations 1–7, 11, 12 and 14, and out-of-band PCE layouts with up to eight distinct speaker positions |

## Behaviour and limits

- `Decoder::decode` takes exactly one ADTS frame including its header. It clears and refills
  `out`, reusing capacity, because rate and layout can change between frames (implicit
  SBR/PS, a new PCE). A frame yields 1024 samples per raw data block, 2048 with SBR.
- On error `out` is empty and the decoder stays usable. Errors are typed:
  `Unsupported(Feature)`, `InvalidData(&str)` naming the syntax element,
  `ResourceLimit(&str)`, `Truncated`, `Budget(StorageError)`.
- Storage that depends on the stream is charged to the decoder's budget before it is
  allocated. In-band reconfiguration (a PCE, a CPE in a mono stream, the start of SBR or
  PS, a coupling channel) builds its whole state, and the larger frame it needs, first.
  A refused charge is `DecodeError::Budget`: the access unit is dropped and nothing of
  the reconfiguration applies, so the next one decodes as if it had not arrived, except
  for an earlier raw data block of the same ADTS frame or a Main-prediction element
  decoded before the refusal. `Decoder::with_limits(Limits { live_storage_bytes })` sets
  the pool, `Decoder::new_in` charges a caller's `Budget`. A layout switch holds at most
  48 elements (16 tags each of SCE, CPE and LFE); the 32 MiB default holds that many at
  318 KiB each (the largest, with SBR and Main prediction), 16 coupling channels of
  53 KiB and 93 planes of four SBR raw data blocks (2.9 MiB): 18.6 MiB in all.
- Refused: SSR (`Feature::SsrGainControl`) and reserved sampling-frequency indices 13–15.
  ER, LD, ELD, USAC and 960-sample frames cannot be carried in ADTS; LATM/LOAS is not
  accepted.
- `ChannelLayout::speakers()` names each plane's position, in the
  [channel order](../codec/README.md#channel-order) of triergon-codec: plane `i` is
  FFmpeg output channel `i`. For ADTS, configurations 5, 6 and 7 map to 5.0(side), 5.1(side) and 7.1.
  PCE layouts that FFmpeg reports as unnamed keep PCE element order.
- Dynamic range control data is parsed and not applied, as FFmpeg does by default.
- FFmpeg-compatible behaviour: the PNS noise generator (LCG, seed `0x1f2e3d4c`); two SCEs
  in configuration 2 map to FL then FR; a CPE in configuration 1 switches to stereo;
  the libfaac fill-element signature skips the first 1024 samples by default.
  Select `compat::priming::Policy::Container` with `Decoder::with_priming_policy`
  to leave that discard to an explicit container edit.

## Known differences from FFmpeg

- A configuration-2 frame that carries a single SCE: FFmpeg leaves FR unwritten, so it
  holds stale buffer contents. This decoder outputs silence on FR. The correctness
  comparison for the pinned `aac-sce-in-stereo.adts` vector initializes both
  reference planes before decoding and checks every sample of both channels.
- Some SBR tables (the noise table and the Huffman books other than SBR_F_ENVBAL30) are not
  transcribed from a document. They are validated by the conformance vectors only;
  codewords no vector reaches are unverified. Provenance:
  [harness/codecs/aac/STATUS.md](../../harness/codecs/aac/STATUS.md).
- Parametric Stereo and SBR: four crafted streams of
  [harness/codecs/aac/ps-open-manifest.json](../../harness/codecs/aac/ps-open-manifest.json)
  are open cases, outside the numeric rules:
  - a PS header without the extension after an error frame while IPD/OPD was on
    (`ps-open-noext-after-error`). From that frame on, FFmpeg's output equals IPD/OPD
    indices 0, while this decoder keeps the earlier values;
  - a time-differential IID envelope after a 4-envelope frame with fewer coded bands
    (`ps-open-dt-envelope-reference`). FFmpeg's dt reference above the coded bands
    differs from this decoder's;
  - the frame after a missing SBR element whose previous frame's envelope crossed the
    frame border (`ps-open-sbr-after-missing`, HE-AAC v1; the same with PS);
  - an SBR payload cut short (`ps-open-sbr-truncated`). This decoder refuses the access
    unit, while FFmpeg outputs the frame.

  The README in `harness/codecs/aac/ps` has the measured differences and what is known of
  each cause.

## Conformance

- Specifications: ISO/IEC 14496-3 (subpart 4 AAC, 4.6.18 SBR); QMF window from ETSI TS 101
  980 Annex I Table I.1; QMF flowcharts from 3GPP TS 26.404 Figure 6 and TS 26.405 Figure 4.
  PS: H. Purnhagen, DAFx 2004 (mixing, hybrid filterbank, decorrelator structure),
  J. Breebaart et al., EURASIP JASP 2005:9 (the mixing for ICC modes 3..5, IPD/OPD phases)
  and 3GPP TS 26.405 (hybrid prototype Table 1, 20-band map Table 2); the `ps_data()`
  syntax, the Huffman books and every table and rule they do not print are recovered from
  FFmpeg's output by scripted probes
  ([harness/codecs/aac/ps](../../harness/codecs/aac/ps/README.md)), regenerated and checked
  by `cargo xtask codecs aac-ps-tables [--check]`. Observed behaviour, given by no
  available text: the decorrelator's lead delay and coefficient taper, the decorrelator
  state kept over the stream, the transient ducking (Purnhagen names transient reduction
  but gives no form), the IPD/OPD smoothing and the behaviour over frames, including a
  frame without its SBR element; the code names the probes of each.
- Reference: FFmpeg 9.0.2's float AAC decoder on the FATE `aac/` vectors (MP4/3GP tracks
  remuxed to ADTS by `cargo xtask codecs derive`,
  [derived-streams.json](../../harness/codecs/derived-streams.json)) and three crafted HE-AAC v2 streams
  ([harness/codecs/aac/ps/vectors](../../harness/codecs/aac/ps/vectors)): a 20/34-band
  change registered while PS is off, IPD/OPD across a 34 -> 20 change, and a PS frame
  without its SBR element. Every sample of every channel is compared with no delay search or gain
  fitting: exact rate, layout and sample count; per channel RMS ≤ 8.8e-6 and
  max |diff| ≤ 6.1035e-5 (`harness/codecs/numeric-rules.json`).
- The `codecs-audio` check passes all 39 AAC vectors on Linux AArch64 and
  32-bit ARMv7 (`thumbv7neon-unknown-linux-gnueabihf`) at `4b87991e`. The
  `aac-sce-in-stereo` comparison checks both float planes and the stereo
  output.
- SBR/PS vectors are also checked against the conformance `.s16` references (≤ 1 LSB).
- `cargo test -p triergon-codec-aac` runs synthetic ADTS and raw AAC-LC tests. `tests/fate_vectors.rs`
  runs the FATE vectors when `.research/fate-samples/aac/` is present (set
  `AAC_REF_DIR` for the float comparison) and skips otherwise.
- Comparison and probe tools: [harness/codecs/aac/tools](../../harness/codecs/aac/tools).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).
