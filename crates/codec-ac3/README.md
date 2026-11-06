# triergon-codec-ac3

AC-3 and E-AC-3 decoder written from ATSC A/52:2018 (main body, Annex D alternate bit
stream information, Annex E). It is a codec-layer crate whose only dependency is
[triergon-codec](../codec/README.md) (`AudioDecoder`, `AudioFrame`, `ChannelLayout`,
`Speaker`, `DownmixLevels`). It decodes one access unit at a time to planar `f32` in FFmpeg's native channel
order and exports FFmpeg-compatible downmix metadata. It does not downmix or resample.

## Usage

```rust
use triergon_codec_ac3::{AudioFrame, Decoder, access_unit_len};

let mut dec = Decoder::new(); // DecoderOptions::default(): ffmpeg_compat on
let mut frame = AudioFrame::default();
let mut pos = 0;
while pos < stream.len() {
    let len = access_unit_len(&stream[pos..])?;
    frame.clear();
    let info = dec.decode(&stream[pos..pos + len], &mut frame)?; // appends samples
    // info.sample_rate, info.layout, info.downmix, info.concealed
    pos += len;
}
```

`Decoder::with_options(DecoderOptions { crc, noise, ffmpeg_compat })` selects the CRC
policy (`CrcPolicy::Report` or `Reject`), the noise mode (`NoiseMode::Normal` or `Off`)
and FFmpeg compatibility. `examples/decode_ac3.rs` writes interleaved f32le.

## Supported

| Area | Coverage |
|---|---|
| AC-3 | bsid 0–8 incl. Annex D alternate syntax; bsid 9/10 at half/quarter sample rate |
| E-AC-3 | bsid 11–16; independent substream 0 with dependent substreams (up to 7.1); 1/2/3/6-block frames |
| Channels | acmod 1/0 to 3/2 with and without LFE, dual mono 1+1 |
| Rates | 32, 44.1, 48 kHz, and the bsid 9/10 rates down to 8 kHz |
| Tools | D15/D25/D45 exponents, parametric and delta bit allocation, all quantizers, coupling with phase flags, rematrixing, block switching, dynamic range (dynrng), E-AC-3 frame exponent strategy, AHT, SPX |

Access units: one AC-3 syncframe, or one E-AC-3 independent substream 0 plus its
dependents. `access_unit_len` splits a stream the same way FFmpeg's parser does.
Further independent programs are skipped.

## Behaviour and limits

- `decode` appends to `out`, which must be empty or hold the same rate and layout
  (`OutputLayoutMismatch` otherwise). `reset` drops delay lines, concealment history and
  noise state.
- Planes follow the triergon-codec [channel order](../codec/README.md#channel-order); 7.1 is
  FL FR FC LFE BL BR SL SR.
- Refused with `Unsupported(Feature)`: bsid > 16, reserved fscod2 and stream type 3, and
  (with `ffmpeg_compat`) E-AC-3 reduced sample rates. Enhanced coupling conceals the frame
  and reports `Feature::EnhancedCoupling` in `FrameInfo::concealed`; FFmpeg does the same.
- Concealment (A/52 7.10 block repeat): a damaged block repeats the previous block for
  the rest of the frame; a truncated AC-3 frame outputs six copies of the last block; a
  truncated E-AC-3 frame outputs nothing.
- CRC failures are decoded and reported (`FrameInfo::crc_ok`) unless `CrcPolicy::Reject`.
- Not applied, as in FFmpeg: dialnorm, compr, transient pre-noise processing (parsed).
- `FrameInfo::downmix` mirrors FFmpeg's downmix side data: exported only for programs
  with more than two channels, taken from the last syncframe of the access unit.
- Memory: about 80 KB per active substream plus about 30 KB of tables and buffers; no
  steady-state allocation.

### FFmpeg compatibility (`ffmpeg_compat`, default on)

The option reproduces FFmpeg 9.0.2 conventions that A/52 leaves open or that differ from
the text:

- Noise: bap-0 dither, AHT zero-bin fill and SPX blending noise use FFmpeg's seed-zero
  noise stream (additive lagged generator, lags 24 and 55) in the same draw order, so
  dithered output matches sample for sample. AHT zero bins are filled regardless of
  dithflag; LFE is never dithered. Without compatibility a private xorshift generator is
  used; `NoiseMode::Off` replaces all noise with zero.
- The first block of a stream that mixes transform lengths rewrites the delay lines as
  FFmpeg does.
- hebap 4 vector quantization (Table E4.4) decodes index `i` with row `i + 1` and index 31
  as zero.
- A dependent substream whose rate or block count differs from the core is dropped and
  the core is output alone (`InvalidData` with compatibility off).
- Reading past the frame end yields zero bits (an error with compatibility off).
- E-AC-3 reduced sample rates are refused (decoded with compatibility off).

Independent of the option: coupling delta bit-allocation offsets count from the first
coupling band (A/52 5.4.3.51); AC-3 bsid 9/10 shifts sdecay/fdecay right by `bsid − 8` and
indexes the hearing threshold by `band >> (bsid − 8)`, as FFmpeg does.

## Known gaps

- No oracle coverage: dual mono, 2/3-block E-AC-3 frames, `blkswe`, AHT on coupling and
  LFE channels, dependents without channel map, E-AC-3 reduced sample rates with
  `ffmpeg_compat` off (its hearing-threshold column is a guess).
- bsid 9/10 rests on self-authored probe streams and patched content; no encoded stream
  of that kind has been compared.
- Error concealment on corrupt mono/stereo/2.1 input can differ from FFmpeg in a few
  frames.

## Conformance

- Specification: ATSC A/52:2018. Tables are generated from the specification text by
  `harness/codecs/ac3/gen_tables.py` (`src/spec_tables.rs`).
- Reference: FFmpeg 9.0.2's float decoder on the FATE AC-3/E-AC-3 vectors and on streams
  from FFmpeg's own encoders covering every acmod, rate, coupling and E-AC-3. Every sample
  is compared: exact rate, layout and count; per channel RMS ≤ 8.8e-6 and
  max |diff| ≤ 6.1035e-5 (`harness/codecs/numeric-rules.json`). Dither-free comparisons use
  streams rewritten by `examples/strip_dither.rs`.
- Downmix metadata is compared per frame against `ffprobe -show_frames`.
- `cargo test -p triergon-codec-ac3` runs table, transform, synthetic-frame, bsid 9/10 and
  corruption tests; FATE checks skip when the corpus is absent.
- Derivation notes and tools: [harness/codecs/ac3](../../harness/codecs/ac3/STATUS.md).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).
