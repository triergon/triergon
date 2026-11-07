# triergon-codec-aac

MPEG-4 AAC decoder for ADTS access units: AAC Main, LC and LTP, HE-AAC (SBR) and
HE-AAC v2 (Parametric Stereo). It is a codec-layer crate that depends on
[triergon-bitstream](../bitstream/README.md) and
[triergon-codec](../codec/README.md), whose `AudioDecoder` trait it implements and
whose `AudioFrame`, `ChannelLayout` and `Speaker` it re-exports. It does not split streams, resample or downmix: the caller hands it one ADTS
frame at a time and receives planar `f32` in FFmpeg's native channel order.

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

## Supported

| Area | Coverage |
|---|---|
| Transport | ADTS, CRC-protected or not, 1–4 raw data blocks per frame (CRC parsed, not checked) |
| Object types | Main (with prediction), LC, LTP |
| Elements | SCE, CPE (common window, M/S), LFE, CCE (dependent and independent coupling), DSE, FIL, PCE incl. height extension |
| Tools | Huffman section/scalefactor/spectral coding, pulse data, TNS, intensity stereo, PNS, sine/KBD windows, all window sequences |
| SBR | Implicit signalling; output at twice the core rate from the first SBR frame; all grid classes, coupled and balanced envelopes, up to 6 patches, limiter, sinusoids |
| PS | Implicit signalling (mono stream becomes stereo); 20- and 34-band hybrid, IID/ICC/IPD/OPD, switching between 20 and 34 bands |
| Rates | Sampling-frequency indices 0–12 |
| Layouts | Channel configurations 1–7 and PCE layouts, up to 48 channels |

## Behaviour and limits

- `decode` takes exactly one ADTS frame including its header. It clears and refills
  `out`, reusing capacity, because rate and layout can change between frames (implicit
  SBR/PS, a new PCE). A frame yields 1024 samples per raw data block, 2048 with SBR.
- On error `out` is empty and the decoder stays usable. Errors are typed:
  `Unsupported(Feature)`, `InvalidData(&str)` naming the syntax element,
  `ResourceLimit(&str)`, `Truncated`.
- Refused: SSR (`Feature::SsrGainControl`) and reserved sampling-frequency indices 13–15.
  ER, LD, ELD, USAC and 960-sample frames cannot be carried in ADTS; LATM/LOAS is not
  accepted.
- `ChannelLayout::speakers()` names each plane's position, in the
  [channel order](../codec/README.md#channel-order) of triergon-codec: plane `i` is
  FFmpeg output channel `i`. Configurations 5, 6 and 7 map to 5.0(side), 5.1(side) and 7.1.
  PCE layouts that FFmpeg reports as unnamed keep PCE element order.
- Dynamic range control data is parsed and not applied, as FFmpeg does by default.
- FFmpeg-compatible behaviour: the PNS noise generator (LCG, seed `0x1f2e3d4c`); two SCEs
  in configuration 2 map to FL then FR; a CPE in configuration 1 switches to stereo;
  the libfaac fill-element signature skips the first 1024 samples.

## Known differences from FFmpeg

- A configuration-2 frame that carries a single SCE: FFmpeg leaves FR unwritten, so it
  holds stale buffer contents. This decoder outputs silence on FR.
- Some SBR and PS tables (SBR noise and Huffman tables, PS Huffman tables, the 12/8/4-band
  hybrid prototypes, the k-to-i maps) are not transcribed from a document. They are
  validated by the conformance vectors only; codewords and paths no vector reaches are
  unverified: SBR Huffman table tails, fine-resolution IID, PS mixing B, IPD/OPD in the
  34-band or 5-band configuration. Provenance:
  [harness/codecs/aac/STATUS.md](../../harness/codecs/aac/STATUS.md).

## Conformance

- Specifications: ISO/IEC 14496-3 (subpart 4 AAC, 4.6.18 SBR, subpart 8 PS); QMF window
  from ETSI TS 101 980 Annex I Table I.1; QMF flowcharts from 3GPP TS 26.404 Figure 6 and
  TS 26.405 Figure 4.
- Reference: FFmpeg 9.0.2's float AAC decoder on the FATE `aac/` vectors (MP4/3GP tracks
  remuxed to ADTS). Every sample of every channel is compared with no delay search or gain
  fitting: exact rate, layout and sample count; per channel RMS ≤ 8.8e-6 and
  max |diff| ≤ 6.1035e-5 (`harness/codecs/numeric-rules.json`).
- SBR/PS vectors are also checked against the conformance `.s16` references (≤ 1 LSB).
- `cargo test -p triergon-codec-aac` runs synthetic ADTS tests; `tests/fate_vectors.rs`
  runs the FATE vectors when `.research/fate-samples/aac/` is present (set
  `AAC_REF_DIR` for the float comparison) and skips otherwise.
- Comparison and probe tools: [harness/codecs/aac/tools](../../harness/codecs/aac/tools).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).
