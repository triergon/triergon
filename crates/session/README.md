# triergon-session

A decode-session layer for playback. It takes AVC/HEVC Annex B access units and
AC-3, E-AC-3, ADTS AAC, DTS core, FLAC, Opus, Vorbis or MPEG audio frames, drives the
pure-Rust codec crates through the [triergon-codec](../codec/README.md) traits, and
publishes clock-gated RGBA pictures letterboxed into a canvas (1920×1080 by default) and
PCM (by default interleaved stereo s16). It is a composition-layer crate over
triergon-codec and [triergon-pixel](../pixel/README.md). The caller owns the clock,
packet framing, output devices and GPU upload. Nothing here spawns a process or links a
native library.

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
    video.load(Some(video_decoder));
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

`VideoSession::with_canvas(limits, canvas)` letterboxes into another `Canvas`, and
`AudioSession::with_form(limits, form)` delivers another `PcmForm`: `Planar` keeps the
decoder's `f32` planes. `on_refusal(OnRefusal::Skip)` makes either lane drop and count an
access unit its decoder refuses instead of failing.

`audio_decoder(codec, configuration)` takes the canonical configuration of FLAC, Opus
and Vorbis (see `audio_configuration`) and ignores it for the others. Without the
`codecs` feature, implement `VideoDecoder` and `AudioDecoder` (re-exported from
triergon-codec) yourself.

## Features

| Feature | Default | Effect |
|---|---|---|
| `codecs` | off | The nine decoder crates behind `video_decoder`, `audio_decoder`, `StreamSelection::load`, `TrackDecoder` and `compat::aac` |
| `hevc-simd` | off | The HEVC decoder's nightly-only `std::simd` kernels; identical output |
| `aac-simd` | off | The [AAC decoder's](../codec-aac/README.md#features) nightly-only `std::simd` SBR QMF banks |

For ADTS packets carrying an explicit container priming edit, use
`compat::aac::adts_decoder(PrimingPolicy::Container)` and apply the edit to
the decoded PCM. The default AAC decoder uses the encoder signature.

Use `TrackDecoder::aac_access_units_in` to decode raw AAC-LC from a checked
AudioSpecificConfig with a fixed layout or a Program Configuration Element.
`TrackDecoder::aac_access_units_with_layout_in` accepts
`compat::aac::Policy::ConventionalSurround` for fixed layouts. Call
`TrackDecoder::into_decoder` to use the configured decoder in an audio session; with
parsed facts for a fixed layout, `AacAccessUnitConfig::decoder_with_layout_in` builds
one directly.

## Supported

| Stream | Input | Output |
|---|---|---|
| Video | H.264, HEVC Annex B access units with optional PTS | RGBA letterboxed into the lane's canvas, from 8- and 10-bit 4:2:0 sources |
| Audio | AC-3, E-AC-3, AAC (ADTS or configured raw LC), DTS core, FLAC, Opus, Vorbis, MPEG audio Layer I-III | the lane's `PcmForm` at the decoder's rate: interleaved stereo s16, or `f32` planes |
| Container track | any `TrackInfo` of a supported codec, its packets | `TrackDecoder`: decoded frames with the container's trims applied |

`TrackDecoder::new(track)` picks the decoder from a track's codec (`audio_codec`) and
configuration (Matroska CodecPrivate, ISO BMFF codec records, Ogg headers). For AAC,
a track with an AudioSpecificConfig takes raw LC packets. An AAC track without
configuration takes ADTS packets. Use `aac_access_units_in` when the packet's sample
description supplies a different configuration.
`TrackDecoder::decode(packet, frame)` drops what the container marks for
discarding: the packet's `Trim`, samples before time zero (Matroska CodecDelay, Ogg
pre-skip) and the decoder's start delay (`AudioDecoder::delay`, the Opus pre-skip), a
start trim carrying into the next packet when it is longer than one.
`Discard::of(packet)` with `AudioSession::feed_discarding` does the same for a
clock-paced session. `examples/decode_audio.rs` decodes the first audio track of any
file `triergon::open_blocking` opens.

The `mix` module holds the stereo downmix and the s16 conversion.

## Behaviour and limits

- **Queues.** `QueueLimits::VIDEO` holds 120 units / 32 MiB (16 MiB per unit);
  `QueueLimits::AUDIO` 256 units / 4 MiB (1 MiB per unit). A full queue
  returns `FeedError::Backpressure` and the caller keeps the unit. Empty or
  oversized units, and units fed after `end_of_stream`, are `FeedError::Invalid`;
  feeding with no decoder or after a failure is `FeedError::Idle`.
- **Video timing.** Each picture keeps the PTS its access unit carried through
  decoder reordering. The session holds at most one converted picture ahead
  of the clock and publishes it when `clock_ns >= pts` (or immediately when
  either is unknown). `Published::sequence` increases across generations so a
  texture cache never confuses streams.
- **Audio timing.** `pull` decodes queued units whose PTS minus `lead_ns` has
  been reached. The caller passes the lead; `audio::DEFAULT_LEAD_NS` is 120 ms.
  A sample-rate change ends the current block.
- **Lifecycle.** Both lanes share one vocabulary. `load(Some(decoder))` starts a new
  stream and `load(None)` unloads the lane (`SessionState::Idle`, and no picture);
  `flush` is a seek: the queue is discarded and the decoder reset, the lane keeps its
  decoder, and the video lane keeps its last picture on screen. Both advance
  `generation()`. `end_of_stream` drains the decoder; the state then becomes
  `SessionState::Ended`. A decoder refusal is `SessionState::Failed` until the
  next load or flush, or under `OnRefusal::Skip` drops that unit; `refusals`
  and `last_refusal` count and name the refusals of the current stream.
- **Letterbox.** Each picture goes through one `triergon_pixel::letterbox::Scaler`
  into the lane's `Canvas`; the fit, padding and conversion rules are in
  [triergon-pixel](../pixel/README.md).
- **Downmix.** `mix::Downmix`: front pair at unity, centre and surrounds at −3 dB,
  LFE dropped, normalised so no output exceeds full scale; mono goes to both sides
  at −3 dB. The `DownmixLevels` an AC-3/E-AC-3 stream carries (mix levels, Dolby or
  Pro Logic II surround encoding) are honoured. A layout without speaker positions
  mixes as FFmpeg's default layout for its channel count, and 16-bit frames (FLAC) mix
  in integers as FFmpeg's resampler mixes them
  ([harness/codecs/downmix-integer.md](../../harness/codecs/downmix-integer.md)). Float
  mixes become s16 rounded toward negative infinity and saturated (`mix::to_s16`).
- **Stream selection.** `StreamSelection` names a stream's video codec and its
  audio codec, if any. `StreamSelection::load` starts both sessions with the
  matching decoders; with `audio: None` the audio session stays `Idle`.

## Conformance

The [clocked video comparison](../../harness/features/video-session-clock/contract.json)
checks AVC and HEVC picture order, exact clock release and retained RGBA canvas
bytes, plus queue and decoder refusals, flush and unload. The existing session
grades compare decoded output and the declared mid-GOP reset behaviour.
The [track reset comparison](../../harness/codecs/session-cases.json) covers
`TrackDecoder` restarts after actual AAC, AC-3, E-AC-3, DTS, FLAC, MPEG
audio, Opus and Vorbis packets, against the reference decoder closed and opened again
at the same packet: a reset drops all signal history and is not the start of a stream,
so an Opus pre-skip, which applies once at the start (RFC 7845 section 4.2), is not
discarded again. The [AAC access-unit comparison](../../harness/features/aac-access-units/contract.json)
covers configured fixed and program layouts, and the clocked video gate
exercises both standalone video factories.
The [audio lifecycle comparison](../../harness/features/audio-session-lifecycle/contract.json)
checks the session's clock lead, cross-packet discard, rate-separated blocks,
drain timestamps, backpressure, reset and both decoder-refusal policies in
addition to the native codec session comparisons.

The letterbox and downmix rules are native observations of FFmpeg 9.0.2, recorded in
[rgba-contract.json](../../harness/codecs/rgba-contract.json) and
[downmix-contract.json](../../harness/codecs/downmix-contract.json). Codec conformance
lives in the codec crates and [harness/codecs](../../harness/codecs/STATUS.md); results
are in [docs/status.md](../../docs/status.md).
