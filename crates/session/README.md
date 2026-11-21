# triergon-session

A decode-session layer for desktop playback. It takes AVC/HEVC Annex B access
units and AC-3, E-AC-3, ADTS AAC, DTS core, FLAC, Opus, Vorbis or MPEG audio frames, drives the pure-Rust codec
crates through the [triergon-codec](../codec/README.md) traits, and publishes
clock-gated RGBA pictures (1920×1080 unless the caller picks another canvas) and
PCM (interleaved stereo s16 unless the caller picks another form). It is a
composition-layer crate over triergon-codec and
[triergon-pixel](../pixel/README.md). The caller owns the clock, packet framing,
output devices and GPU upload. Nothing here spawns a process or links a native
library.

## Usage

```rust
use triergon_session::{
    AudioSession, DynAudioDecoder, DynVideoDecoder, FeedError, PcmBlock, QueueLimits,
    VideoSession, audio,
};

// With the `codecs` feature the decoders come from `video_decoder(VideoCodec::H264)` and
// `audio_decoder(AudioCodec::Aac)`; `StreamSelection::load` starts both sessions with them.
fn play(
    video_decoder: Box<DynVideoDecoder>,
    audio_decoder: Option<Box<DynAudioDecoder>>,
    access_unit: &[u8],
    pts_ns: i64,
    clock_ns: i64,
) -> Result<(), FeedError> {
    let mut video = VideoSession::new(QueueLimits::VIDEO);
    video.load(video_decoder);
    let mut audio = AudioSession::new(QueueLimits::AUDIO);
    audio.load(audio_decoder);

    video.feed(access_unit, Some(pts_ns))?;     // Err(Backpressure): keep and retry
    if video.step(Some(clock_ns)) {
        if let Some(picture) = video.latest() { /* sequence, generation, pts, rgba */ }
    }
    let mut pcm = PcmBlock::default();
    audio.pull(Some(clock_ns), audio::DEFAULT_LEAD_NS, &mut pcm);
    Ok(())
}
```

`video_decoder`, `audio_decoder` and `StreamSelection::load` need the `codecs`
feature, which adds the nine decoder crates. `video_codec` and `audio_codec` map
a container track's codec to a decoder lane. `audio_decoder(codec, configuration)` takes
the canonical configuration of FLAC, Opus and Vorbis (see `audio_configuration`) and
ignores it for the others. Each decoder charges its storage to a pool of its codec's
default limit; `video_decoder_in` and `audio_decoder_in` charge the caller's `Budget`
instead, so several decoders can share one limit. Without it, implement `VideoDecoder`
and `AudioDecoder` (re-exported from triergon-codec) with `Error = triergon_session::Error`
yourself, or box a decoder with another error type through `boxed()`; the tests use
scripted decoders. The `hevc-simd` and `aac-simd` features turn on the HEVC and AAC
decoders' nightly-only SIMD kernels (their `simd` features).

For ADTS packets carrying an explicit container priming edit, use
`compat::aac::adts_decoder(PrimingPolicy::Container)` and apply the edit to
the decoded PCM. The default AAC decoder uses the encoder signature.

Use `TrackDecoder::aac_access_units_in` to decode raw AAC-LC from a checked
AudioSpecificConfig with a fixed layout or a Program Configuration Element.
`TrackDecoder::aac_access_units_with_layout_in` accepts
`compat::aac::Policy::ConventionalSurround` for fixed layouts. Call
`TrackDecoder::into_decoder` to use the configured decoder in an audio session.
With parsed facts for a fixed layout, `AacAccessUnitConfig::decoder_with_layout_in`
also builds a decoder for an audio session.

## Supported

| Stream | Input | Output |
|---|---|---|
| Video | H.264, HEVC Annex B access units with optional PTS | RGBA letterboxed into a `Canvas` (1920×1080 by default), 8- and 10-bit 4:2:0 sources |
| Audio | AC-3, E-AC-3, AAC (ADTS or configured raw LC), DTS core, FLAC, Opus, Vorbis, MPEG audio Layer I-III | at the decoder's rate: interleaved stereo s16 (`AudioSession::new`), or the decoder's `f32` planes as they are (`AudioSession::with_form(limits, Planar)`) |
| Container track | any `TrackInfo` of a supported codec, its packets | `TrackDecoder`: decoded frames with the container's trims applied |

`TrackDecoder::new(track)` picks the decoder from a track's codec (`audio_codec`) and
configuration (Matroska CodecPrivate, ISO BMFF codec records, Ogg headers). The
generic AAC track decoder takes ADTS. For raw packets, call
`TrackDecoder::aac_access_units_in` with the AudioSpecificConfig of the packet's
sample description. Callers with parsed facts for a fixed layout can construct an
`AacAccessUnitConfig` and build the same decoder for an `AudioSession`.
`TrackDecoder::decode(packet, frame)` drops what the container marks for
discarding: the packet's `Trim`, samples before time zero (Matroska CodecDelay, Ogg
pre-skip) and the decoder's start delay (`AudioDecoder::delay`, the Opus pre-skip), a
start trim carrying into the next packet when it is longer than one. `Discard::of(packet)`
with `AudioSession::feed_discarding` does the same for a clock-paced session.
`examples/decode_audio.rs` decodes the first audio track of any file `triergon::open_blocking`
opens, as the harness grades it.

The `mix` module holds the stereo downmix (`mix::Downmix`) and the s16 conversion
(`mix::to_s16`) on their own.

## Behaviour and limits

- **Queues.** `QueueLimits::VIDEO` holds 120 units / 32 MiB (16 MiB per unit);
  `QueueLimits::AUDIO` 256 units / 4 MiB (1 MiB per unit). A full queue
  returns `FeedError::Backpressure` and the caller keeps the unit. Empty or
  oversized units, and units fed after `end_of_stream`, are `FeedError::Invalid`;
  feeding with no decoder or after a failure is `FeedError::Idle`. The error
  keeps these variants when propagated through a standard error trait object.
- **Video timing.** Each picture keeps the PTS its access unit carried through
  decoder reordering. The session holds at most one converted picture ahead
  of the clock and publishes it when `clock_ns >= pts` (or immediately when
  either is unknown). `Published::sequence` increases across generations so a
  texture cache never confuses streams.
- **Audio timing.** `pull` decodes queued units whose PTS minus `lead_ns` has
  been reached. The caller passes the lead; `audio::DEFAULT_LEAD_NS` is 120 ms.
  A sample-rate change ends the current block.
- **Lifecycle.** `load` starts a new stream; video `flush` restarts the timeline
  but keeps the last picture; video `stop` unloads the decoder and clears the picture;
  audio `stop` discards queued input and keeps the decoder. Each of these advances
  `generation()`. `end_of_stream` drains the decoder once every queued unit is decoded:
  the video decoder's remaining pictures, the audio decoder's held-back samples
  (`AudioDecoder::flush`, the Opus resampler delay), which follow the last unit's in its
  block, or start a block without a PTS. The state becomes `SessionState::Ended` once
  that output is delivered.
- **Refusals.** By default a refused access unit fails its lane: `SessionState::Failed`
  with the decoder's error until the next load, flush or stop, the queue discarded.
  A lane built with `.on_refusal(OnRefusal::Skip)` drops the unit and decodes the next
  one instead; a refused end-of-stream drain then ends the lane. Either way
  `refusals()` counts the refusals since the lane last restarted and `last_refusal()`
  holds the latest.
- **Letterbox.** Each picture goes through one `triergon_pixel::letterbox::Scaler`
  into the lane's RGBA canvas: 1920×1080 for `VideoSession::new`, any `Canvas` for
  `VideoSession::with_canvas`. The fit, padding and conversion rules are in
  [triergon-pixel](../pixel/README.md).
- **Output forms.** A `PcmForm` turns decoded frames into the samples of a
  `PcmBlock<F::Samples>` and says which frames continue a block. `mix::Downmix` is the
  default: `PcmBlock` holds `Vec<i16>`. `Planar` keeps the decoder's planes (`Planes`:
  the layout and one `f32` plane per speaker) and starts a new block when the layout
  changes. Another form, such as a stereo `f32` mix, is a `PcmForm` of the caller's.
- **Downmix.** `mix::Downmix`: front pair at unity, centre and surrounds at −3 dB,
  LFE dropped, normalised so no output exceeds full scale; mono goes to both sides
  at −3 dB. The `DownmixLevels` an AC-3/E-AC-3 stream carries (mix levels, Dolby or
  Pro Logic II surround encoding) are honoured. A layout without speaker positions
  mixes as FFmpeg's default layout for its channel count, and 16-bit frames (FLAC) mix
  in integers as FFmpeg's resampler mixes them ([harness/codecs/downmix-integer.md](../../harness/codecs/downmix-integer.md)).
  Float mixes become s16 rounded toward negative infinity and saturated (`mix::to_s16`),
  with a conversion, a comparison and integer saturation: no `floorf` call on 32-bit
  ARM, which has no rounding instruction. On 32-bit ARM a stereo pair that passes
  through converts from the samples' bits with integer operations, which vectorise
  there (LLVM emits no vector float code for that target); the values are the same.
- **Stream selection.** `StreamSelection` names a stream's video codec and its
  audio codec, if any. `StreamSelection::load` starts both sessions with the
  matching decoders; with `audio: None` the audio session stays `Idle`.

## Conformance

The `codecs-files` comparison exercises track selection and packet decoding through
`TrackDecoder::new` and `decode`. The `aac-access-units` comparison exercises
`TrackDecoder::aac_access_units_with_layout_in` and compares decoded sample planes and
speaker order.

The letterbox and downmix rules are native observations of FFmpeg 9.0.2,
recorded as `RGBA-CONTRACT.json` and `DOWNMIX-CONTRACT.json` in the research
tree; `triergon_pixel::letterbox` and `mix.rs` implement them and their unit tests
pin them. The letterbox is checked against the reference chain in
[triergon-pixel](../pixel/README.md#conformance); `tests/sessions.rs` covers
timing, flush, stop, backpressure, failure, rate changes and downmix with
scripted decoders. A host test in `triergon-bridge` (`tests/it/mp4_hevc_aac.rs`)
reads an AVC/AAC MP4, feeds both sessions through the framing adapters, and
checks output positions, seek-to-start replay and teardown. Codec conformance
lives in the codec crates. Gates: [docs/quality.md](../../docs/quality.md).

The [session queue workloads](../../harness/session-queues/README.md) exercise borrowed feeds, byte-limit backpressure, starvation and EOS with decoded AC-3, AAC and AVC output. They compare allocation calls, retained heap, peak heap, RSS and runtime across worker builds.
