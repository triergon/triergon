# triergon-session

A decode-session layer for desktop playback. It takes AVC/HEVC Annex B access
units and AC-3, E-AC-3, ADTS AAC or DTS core frames, drives the pure-Rust codec
crates through the [triergon-codec](../codec/README.md) traits, and publishes
clock-gated 1920×1080 RGBA pictures and interleaved stereo s16 PCM. It is a
composition-layer crate over triergon-codec and
[triergon-pixel](../pixel/README.md). The caller owns the clock, packet framing,
output devices and GPU upload. Nothing here spawns a process or links a native
library.

## Usage

```rust
use triergon_session::{AudioSession, PcmBlock, QueueLimits, VideoSession, audio_decoder, select, video_decoder};

let codecs = select(load_payload);
let mut video = VideoSession::new(QueueLimits::VIDEO);
video.load(video_decoder(codecs.video));
let mut audio = AudioSession::new(QueueLimits::AUDIO);
audio.load(codecs.audio.map(audio_decoder));

video.feed(&access_unit, Some(pts_ns))?;      // Err(Backpressure): keep and retry
if video.step(Some(clock_ns)) {
    let picture = video.latest().unwrap();     // sequence, generation, pts, rgba
}
let mut pcm = PcmBlock::default();
audio.pull(Some(clock_ns), triergon_session::audio::lead_ns(None), &mut pcm);
```

`video_decoder` and `audio_decoder` need the `codecs` feature, which adds the five
decoder crates. Without it, implement `VideoDecoder` and `AudioDecoder` (re-exported
from triergon-codec) yourself; the tests use scripted decoders. The `simd` feature
forwards to the HEVC decoder's nightly-only SIMD kernels.

## Supported

| Stream | Input | Output |
|---|---|---|
| Video | H.264, HEVC Annex B access units with optional PTS | 1920×1080 RGBA, letterboxed, 8- and 10-bit 4:2:0 sources |
| Audio | AC-3, E-AC-3, AAC (ADTS), DTS core | interleaved stereo s16 at the decoder's rate |

Supporting modules: `mix` (stereo downmix and s16 conversion), `queue` (bounded
input), `load` (codec selection from a load payload).

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
  been reached. `audio::lead_ns` parses a millisecond override, defaulting to
  120 ms and clamped to 0–2000 ms. A sample-rate change ends the current block.
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
  Pro Logic II surround encoding) are honoured.
- **Codec selection.** `load::select` picks HEVC when the lowercased payload
  contains `h265` or `hevc`, otherwise H.264. Audio comes from the first
  literal `"audio":"…"` value: `AC3`, `AC3 PLUS`, `AAC` or `DTS`; anything else
  selects no audio.

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
