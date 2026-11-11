# triergon-session

A decode-session layer for desktop playback. It takes AVC/HEVC Annex B access
units and AC-3, E-AC-3, ADTS AAC, DTS core, FLAC, Opus, Vorbis or MPEG audio frames, drives the pure-Rust codec
crates through the [triergon-codec](../codec/README.md) traits, and publishes
clock-gated 1920×1080 RGBA pictures and interleaved stereo s16 PCM. It is a
composition-layer crate over triergon-codec and
[triergon-pixel](../pixel/README.md). The caller owns the clock, packet framing,
output devices and GPU upload. Nothing here spawns a process or links a native
library.

## Usage

```rust
use triergon_session::{
    AudioDecoder, AudioSession, FeedError, PcmBlock, QueueLimits, VideoDecoder, VideoSession, audio,
};

// With the `codecs` feature the decoders come from `video_decoder(VideoCodec::H264)` and
// `audio_decoder(AudioCodec::Aac)`; `StreamSelection::load` starts both sessions with them.
fn play(
    video_decoder: Box<dyn VideoDecoder>,
    audio_decoder: Option<Box<dyn AudioDecoder>>,
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
feature, which adds the nine decoder crates. `audio_decoder(codec, configuration)` takes
the canonical configuration of FLAC, Opus and Vorbis (see `audio_configuration`) and
ignores it for the others. Without it, implement `VideoDecoder`
and `AudioDecoder` (re-exported from triergon-codec) yourself; the tests use
scripted decoders. The `simd` feature forwards to the HEVC decoder's nightly-only
SIMD kernels.

## Supported

| Stream | Input | Output |
|---|---|---|
| Video | H.264, HEVC Annex B access units with optional PTS | 1920×1080 RGBA, letterboxed, 8- and 10-bit 4:2:0 sources |
| Audio | AC-3, E-AC-3, AAC (ADTS or configured raw LC), DTS core, FLAC, Opus, Vorbis, MPEG audio Layer I-III | interleaved stereo s16 at the decoder's rate |
| Container track | any `TrackInfo` of a supported codec, its packets | `TrackDecoder`: decoded frames with the container's trims applied |

`TrackDecoder::new(track)` picks the decoder from a track's codec (`audio_codec`) and
configuration (Matroska CodecPrivate, ISO BMFF codec records, Ogg headers). An AAC
track with AudioSpecificConfig uses its raw access units and fixed channel layout.
An AAC track with empty configuration uses ADTS. Callers with parsed ASC facts
can construct an `AacAccessUnitConfig` and build the same decoder for an `AudioSession`.
`TrackDecoder::decode(packet, frame)` drops what the container marks for
discarding: the packet's `Trim`, samples before time zero (Matroska CodecDelay, Ogg
pre-skip) and the decoder's start delay (`AudioDecoder::delay`, the Opus pre-skip), a
start trim carrying into the next packet when it is longer than one. `Discard::of(packet)`
with `AudioSession::feed_discarding` does the same for a clock-paced session.
`examples/decode_audio.rs` decodes the first audio track of any file `triergon::open_blocking`
opens, as the harness grades it.

Supporting modules: `mix` (stereo downmix and s16 conversion), `queue` (bounded
input).

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
- **Lifecycle.** `load` starts a new stream; video `flush` restarts the timeline
  but keeps the last picture; `stop` clears everything. Each of these advances
  `generation()`. `end_of_stream` drains the decoder; the state then becomes
  `SessionState::Ended`. A decoder error is `SessionState::Failed` until the
  next load, flush or stop.
- **Letterbox.** Each picture goes through one `triergon_pixel::letterbox::Scaler`
  into a 1920×1080 RGBA canvas; the fit, padding and conversion rules are in
  [triergon-pixel](../pixel/README.md).
- **Downmix.** `mix::Downmix`: front pair at unity, centre and surrounds at −3 dB,
  LFE dropped, normalised so no output exceeds full scale; mono goes to both sides
  at −3 dB. The `DownmixLevels` an AC-3/E-AC-3 stream carries (mix levels, Dolby or
  Pro Logic II surround encoding) are honoured. A layout without speaker positions
  mixes as FFmpeg's default layout for its channel count, and 16-bit frames (FLAC) mix
  in integers as FFmpeg's resampler mixes them ([harness/codecs/downmix-integer.md](../../harness/codecs/downmix-integer.md)).
- **Stream selection.** `StreamSelection` names a stream's video codec and its
  audio codec, if any. `StreamSelection::load` starts both sessions with the
  matching decoders; with `audio: None` the audio session stays `Idle`.

## Conformance

The letterbox and downmix rules are native observations of FFmpeg 9.0.2,
recorded as `RGBA-CONTRACT.json` and `DOWNMIX-CONTRACT.json` in the research
tree; `triergon_pixel::letterbox` and `mix.rs` implement them and their unit tests
pin them. The letterbox is checked against the reference chain in
[triergon-pixel](../pixel/README.md#conformance); `tests/sessions.rs` covers
timing, flush, stop, backpressure, failure, rate changes and downmix with
scripted decoders. A host test in `triergon-playback` (`tests/continuation.rs`)
reads an AVC/AAC MP4, feeds both sessions through the framing adapters, and
checks output positions, seek-to-start replay and teardown. Codec conformance
lives in the codec crates. Gates: [docs/quality.md](../../docs/quality.md).
