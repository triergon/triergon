# triergon-codec-opus

Opus decoder written from RFC 6716 with the RFC 8251 updates (its reference code is
normative where the text defers to it), with the channel mapping of RFC 7845. It decodes
one Opus packet per access unit to planar `f32` at 48 kHz in FFmpeg's channel order (the
[triergon-codec](../codec/README.md) `AudioDecoder` contract): SILK-only, hybrid and
CELT-only frames of 2.5 to 60 ms, packets up to 120 ms, and multistream packets of up to
255 streams. It depends on [triergon-codec](../codec/README.md) (`AudioFrame`, layouts,
the shared inverse MDCT), [triergon-core](../core/README.md) (budgets) and
[triergon-framing](../framing/README.md) (`OpusHead`).

## Usage

```rust
use triergon_codec::{AudioDecoder, AudioFrame};
use triergon_codec_opus::{DecodeError, Decoder, Limits};

fn decode(opus_head: &[u8], packets: &[&[u8]]) -> Result<(), DecodeError> {
    // The identification header: Ogg's first packet, Matroska's CodecPrivate.
    let mut decoder = Decoder::from_configuration(opus_head, Limits::default())?;
    let mut frame = AudioFrame::default();
    for packet in packets {
        if decoder.decode(packet, &mut frame).is_ok() {
            // frame.samples_per_channel samples at 48 kHz in frame.planes.
        }
    }
    // The last samples the SILK resampler held back.
    decoder.flush(&mut frame)?;
    Ok(())
}
```

`Decoder::from_configuration_in` charges a caller's `Budget` instead of its own pool.
`examples/decode_opus` decodes the first Opus track of a file read through the `triergon`
facade (Matroska/WebM, Ogg), or an RFC 6716 `.bit` test vector (whose recorded final
range it checks per packet), to interleaved f32le: the decoder's whole output, without
the pre-skip or other container trims.

## Supported

- Packets (RFC 6716 section 3): frame packing codes 0 to 3, constant and variable
  bitrate, padding, and the self-delimiting framing of all but the last stream of a
  multistream packet. Packets breaking rules R1 to R7 are refused.
- CELT (section 4.3): coarse, fine and final energy; bit allocation with dynamic boosts,
  trim, band skipping, intensity and dual stereo; PVQ with spreading, splitting and
  time-frequency changes; transient short blocks; folding, including the hybrid folding
  of RFC 8251 section 9; anti-collapse; the post-filter with its cross-fades;
  de-emphasis; 2.5 to 20 ms frames, bandwidths narrowband to fullband, the hybrid start
  band, mono and stereo frames into mono or stereo streams, and the intensity-stereo
  phase inversion.
- SILK (section 4.2) in `src/silk`, resampled to 48 kHz as FFmpeg does.
- Mode switching (section 4.5): redundant CELT frames in both directions with their
  cross-fades, layer resets on mode changes, and bandwidth and channel-count changes
  between packets.
- Channel mapping families 0 and 1 (Vorbis order, reported in FFmpeg's native layouts:
  mono, stereo, 3.0, quad, 5.0, 5.1, 6.1, 7.1) and 255 (coded order without speaker
  positions, `ChannelLayout::mask() == 0`), coupled and uncoupled streams, stream
  channels used by several outputs, and silent outputs (mapping index 255). Other
  families are `Unsupported`.

## Behaviour and limits

- The header's output gain is applied. Its pre-skip is not: the container trims the
  first `pre_skip` samples, and the end of the stream (Ogg's final granule position,
  Matroska's DiscardPadding), as FFmpeg does unless asked not to.
- Following FFmpeg 9.0.2:
  - SILK output passes through FFmpeg's resampler, which holds back the last 24
    samples of the first narrowband frame after a start and releases them before the
    next frame once SILK stops or changes rate, or at `Decoder::flush`. Packet sizes
    follow (936 for a first 20 ms narrowband frame, 984 when the rate changes). A fade
    into a redundant frame that reaches into held-back samples is completed on their
    release.
  - Frames of zero or one byte decode as CELT or SILK frames (a CELT silence frame for
    zero bytes) rather than through the reference's packet loss concealment; mode
    switches without a redundant frame cross-fade nothing, and a SILK frame after a
    hybrid one gets no CELT fade-out (the reference decodes a silent CELT frame there).
  - Anti-collapse noise is drawn for every band of the first channel, then the second
    (the reference alternates the channels per band; both are within the conformance
    tolerance).
- No packet loss concealment or forward error correction: every access unit is a packet.
- Malformed packets return `DecodeFailure` (an `Invalid` error inside) and leave the
  decoder usable; streams of one multistream packet with different durations are
  `Invalid`.
- Memory: the per-stream state (about 46 KiB: CELT history and energies, SILK state)
  and the output frame are charged to the decoder's budget before they are allocated
  (`Limits::live_storage_bytes`, 64 MiB by default: 255 streams take about 12 MiB and the
  largest frame, 255 channels of 120 ms, 6 MiB); a refused charge is
  `DecodeError::Budget`. The transforms and per-packet scratch are fixed
  at construction (under 200 KiB). Nothing is allocated per packet.

Constant tables are taken from RFC 6716; see [THIRD-PARTY.md](THIRD-PARTY.md).
