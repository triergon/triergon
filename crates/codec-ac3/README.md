# triergon-codec-ac3

AC-3 and E-AC-3 decoder and optional AC-3 encoder written from
[ATSC A/52:2018](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf).
The decoder covers the main body, Annex D alternate bit stream information and
Annex E. It is a codec-layer crate that depends on
[triergon-codec](../codec/README.md) (`AudioDecoder`, `AudioFrame`, `ChannelLayout`,
`Speaker`, `DownmixLevels`) and [triergon-bitstream](../bitstream/README.md). It decodes one access unit at a time to planar `f32` in FFmpeg's native channel
order and exports FFmpeg-compatible downmix metadata. It does not downmix or resample.

## Usage

```rust
use triergon_codec_ac3::{AudioFrame, DecodeError, Decoder, access_unit_len};

fn decode_stream(stream: &[u8]) -> Result<(), DecodeError> {
    let mut dec = Decoder::new(); // DecoderOptions::default(): Compat::Reference
    let mut frame = AudioFrame::default();
    let mut pos = 0;
    while pos < stream.len() {
        let len = access_unit_len(&stream[pos..])?;
        frame.clear();
        let info = dec.decode(&stream[pos..pos + len], &mut frame)?; // appends samples
        // info.sample_rate, info.layout, info.downmix, info.concealed
        pos += len;
    }
    Ok(())
}
```

`Decoder::with_limits(Limits { live_storage_bytes })` gives the decoder its own storage
pool (it fails only for a limit below a pool's own charge) and `Decoder::new_in(&budget)`
charges a caller's `Budget`. `.with_options(DecoderOptions { crc, noise, compat })` then
selects the CRC policy (`CrcPolicy::Report` or `Reject`), the noise mode
(`NoiseMode::Normal` or `Off`) and the output conventions (`Compat::Reference`, the
default, or `Compat::Specification`). `examples/decode_ac3.rs` writes interleaved f32le.

### Encoding PCM

Enable the `encode` feature for AC-3 syncframes at 32, 44.1 or 48 kHz. The encoder
uses adaptive bandwidth, exponent grouping and bit allocation. It reuses each
critical band's exponent counts across bit allocation trials. The masking
threshold rises for bands below the dB-per-bit knee.
When the highest SNR offset leaves space in the frame, the encoder can lower
the masking floor to give quieter coefficients more precision. Long transforms
keep frequency resolution on steady signals. Each full-bandwidth channel uses short transforms
at sharp attacks and silence boundaries. A gradual onset in an otherwise steady
frame can use a long transform to reduce energy outside the coded bandwidth.
The encoder sends fresh exponents at transform changes and silence boundaries. Wide
exponent sets for a single block use D45, and shared wide sets use D25. The
LFE uses long transforms throughout. A short transform skips each silent half.
Blocks with silent input and silent overlap need no transform. A frame with
silence throughout its input and history needs no mantissas. Bit allocation
fits the opening blocks and final mantissas into the word-aligned AC-3 frame
partitions (A/52:2018 §§5.5, 7.10.1). Skip fields place padding before the
final mantissas when needed. The encoder supports mono at 192 kbit/s, stereo at 256 kbit/s and 5.1
at 640 kbit/s. The input plane order for 5.1 is FL, FR, FC, LFE, SL, SR.

```rust
# #[cfg(feature = "encode")]
use triergon_codec_ac3::encoder::{
    ChannelMode, EncodeError, Encoder, MAX_FRAME_BYTES, SAMPLES_PER_FRAME,
};

# #[cfg(feature = "encode")]
fn encode_stereo(
    frames: &[[[f32; SAMPLES_PER_FRAME]; 2]],
    mut emit: impl FnMut(&[u8]),
) -> Result<(), EncodeError> {
    let mut encoder = Encoder::new(48_000, ChannelMode::Stereo)?;
    let mut output = [0; MAX_FRAME_BYTES];
    for [left, right] in frames {
        let bytes = encoder.encode(&[left, right], &mut output)?;
        emit(&output[..bytes]);
    }
    Ok(())
}
```

Keep the same encoder between frames. It reuses its working storage, with
transform coefficients stored only for the configured channels. Each call
consumes 1,536 samples per plane in the range −1 to +1 and can vary in output
size at 44.1 kHz. The transform starts with 256 samples of silent history. Append
zero samples at the end to flush the delayed audio and trim the decoded result
to the original length. `reset` clears the history and restarts the frame-size
clock.

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
  (with `Compat::Reference`) E-AC-3 reduced sample rates. Enhanced coupling conceals the frame
  and reports `Feature::EnhancedCoupling` in `FrameInfo::concealed`; FFmpeg does the same.
- Concealment (A/52 7.10 block repeat): a damaged block repeats the previous block for
  the rest of the frame; a truncated AC-3 frame outputs six copies of the last block; a
  truncated E-AC-3 frame outputs nothing.
- CRC failures are decoded and reported (`FrameInfo::crc_ok`) unless `CrcPolicy::Reject`.
- Not applied, as in FFmpeg: dialnorm, compr, transient pre-noise processing (parsed).
- `FrameInfo::downmix` mirrors FFmpeg's downmix side data: exported only for programs
  with more than two channels, taken from the last syncframe of the access unit.
- Memory: about 80 KB per active substream plus about 30 KB of tables and buffers; no
  steady-state allocation. Each dependent substream's state and the output frame the
  decoder sizes are charged to its budget first (`Limits::live_storage_bytes`, 4 MiB by
  default); a refused charge is `DecodeError::Budget` and the decoder stays usable.

### FFmpeg compatibility (`Compat::Reference`, the default)

`Compat::Reference` reproduces FFmpeg 9.0.2 conventions that A/52 leaves open or that
differ from the text:

- Noise: bap-0 dither, AHT zero-bin fill and SPX blending noise use FFmpeg's seed-zero
  noise stream (additive lagged generator, lags 24 and 55) in the same draw order, so
  dithered output matches sample for sample. AHT zero bins are filled regardless of
  dithflag; LFE is never dithered. With `Compat::Specification` a private xorshift
  generator is used; `NoiseMode::Off` replaces all noise with zero.
- The first block of a stream that mixes transform lengths rewrites the delay lines as
  FFmpeg does.
- hebap 4 vector quantization (Table E4.4) decodes index `i` with row `i + 1` and index 31
  as zero.
- A dependent substream whose rate or block count differs from the core is dropped and
  the core is output alone (`InvalidData` with `Compat::Specification`).
- Reading past the frame end yields zero bits (an error with `Compat::Specification`).
- E-AC-3 reduced sample rates are refused (decoded with `Compat::Specification`).

In both modes: coupling delta bit-allocation offsets count from the first coupling band
(A/52 5.4.3.51); AC-3 bsid 9/10 shifts sdecay/fdecay right by `bsid − 8` and indexes the
hearing threshold by `band >> (bsid − 8)`, as FFmpeg does.

## Known gaps

- No oracle coverage: dual mono, 2/3-block E-AC-3 frames, `blkswe`, AHT on coupling and
  LFE channels, dependents without channel map, E-AC-3 reduced sample rates with
  `Compat::Specification` (its hearing-threshold column is a guess).
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
- Gate item `codecs-audio` grades the FATE AC-3 and E-AC-3 vectors and the AC-3 streams
  copied out of FATE's WAV and transport stream on Linux AArch64 and 32-bit ARMv7
  (`thumbv7neon-unknown-linux-gnueabihf`): 6/6 AC-3 and 6/6 E-AC-3 pass on both at
  `c68c9667`, with FFmpeg's noise reproduced.
- Downmix metadata is compared per frame against `ffprobe -show_frames`.
- `cargo test -p triergon-codec-ac3` runs table, transform, synthetic-frame, bsid 9/10 and
  corruption tests; FATE checks skip when the corpus is absent.
- With `--features encode`, the tests check frame sizes, CRCs, channel order, signal
  alignment and error recovery across the encoder's nine rate and layout combinations.
  The transform tests cover both lengths and the overlap between them. A surround
  test checks that an attack in one channel leaves the preceding silent block
  quiet without disrupting the other planes.
  The ignored `ffmpeg_interoperability_and_signal_quality` test requires
  `TRIERGON_FFMPEG` and a new directory in `TRIERGON_AC3_OUTPUT`. It decodes the
  generated tone, broadband and attack streams with FFmpeg. It prints per-channel
  signal-to-noise ratios at the same bitrate and the peak level in the last
  128 samples of the block before an attack. Broader signal quality and native
  performance measurements remain open.
- The [encoder measurement contract](../../harness/features/ac3-encode/contract.json)
  runs all 27 hash-pinned cases on the same preloaded PCM. The
  `encoder_workers_decode_full_sessions` test decodes the full streams emitted
  by the measured workers. See [encoder measurements](../../docs/quality.md#ac-3-encoding).
- Derivation notes and tools: [harness/codecs/ac3](../../harness/codecs/ac3/STATUS.md).
  Project-wide gates: [docs/quality.md](../../docs/quality.md).
