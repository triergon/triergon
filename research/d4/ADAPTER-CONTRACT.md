# D4 / O61 simulator adapter contract (replacement for `sim_video.rs` / `sim_audio.rs`)

Status: implemented session layer (`crates/sim`) with executable lifecycle tests;
decoders in `crates/codec-{h264,hevc,ac3,aac,dca}`. Streem edits, device output and
target integration belong to the parent. Nothing here is a qualification claim.

## Source pins (read-only)

- native-jelly-streem personal/main `3d0fca404b69e3e13f261a4cbc71a6e8db57f934`:
  `rust-modules/src/player/sim_video.rs`, `sim_audio.rs`, `ffi_host.rs` (video + audio lanes).
- upstream `3bc30f4edd3b392724968f9945dbef002cd646f2`: only
  `rust-modules/media/src/player/sim_video.rs` (moved; no audio lane).
- Earlier inventory `554f39f6…` is not an ancestor of either head; its behaviour matches
  personal/main apart from the `nj_*`→`streem_*` renames and env names
  (`NJ_SIM_*` → `STREEM_SIM_*`; upstream uses `PLXNATIVE_SIM_FFMPEG`).

## What the consumer adapter calls

| Streem call (source) | Replacement |
|---|---|
| `sim_video::load(payload, clock)` | `let sel = triergon_sim::select(payload); video.load(decoder_for(sel.video)); audio.load(sel.audio.map(decoder_for))` |
| `sim_video::feed(au, pts)` (`es_data == 1`) | `video.feed(au, Some(pts))` → `Err(Backpressure)` means keep the AU and retry after `step`; the source's unbounded channel is gone |
| `sim_audio::feed(au, pts)` (`es_data == 2`) | `audio.feed(au, Some(pts))`, same backpressure rule |
| reader thread waiting on clock | `video.step(Some(clock()))` from the adapter's pump (render or worker thread); returns true when a new picture was published |
| `composite_under()` reading `LATEST` | `video.latest()` → `Published { sequence, generation, pts, rgba: Arc<Vec<u8>> }`, 1920×1080×4 |
| writer thread pacing to `clock - lead` | `audio.pull(Some(clock()), lead_ns(env STREEM_SIM_AUDIO_LEAD_MS), &mut block)` → interleaved stereo s16 at `block.sample_rate` for the device writer |
| `sim_video::flush()` | `video.flush()` (keeps the picture; new generation; decoder state dropped) |
| `sim_audio::stop()` on flush | `audio.stop()` |
| `sim_video::stop()` / unload / destroy / quarantine | `video.stop(); audio.stop()` (picture cleared) |
| `sf_push_eos` (source: no-op for decoders) | `video.end_of_stream(); audio.end_of_stream()` — decoded EOF becomes observable as `SessionState::Ended` |

Selection reproduces the frozen source rules exactly (`load.rs` tests): video is HEVC iff the
lowercased payload contains `h265`/`hevc`, else H.264; audio is the literal `"audio":"`
value `AC3`/`AC3 PLUS`/`AAC`/`DTS`, otherwise no audio lane. A VP9/AV1 route therefore
selects the H.264 lane, as in the source; the replacement's H.264 decoder then reports
`SessionState::Failed(..)` instead of a silent black picture. No inferred codec exists.

## States and distinctions

`SessionState::{Idle, Running, Starved, Ended, Failed(reason)}`:
starvation (queue empty, waiting for input) is distinct from decoded EOF (`end_of_stream`
then every picture/sample delivered) and from decoder failure (typed reason, sticky until
the next `load`/`flush`/`stop`). Feeding after `end_of_stream` is refused (`Invalid`).

## Timing

- Each picture carries the PTS of the access unit that coded it through reordering
  (decoder-propagated), not the source's min-heap association. Unknown PTS (`None`)
  publishes immediately in output order. Signed PTS are kept as given.
- A picture is published once `clock >= pts`; at most one converted picture waits ahead
  of the clock (the source reader also held one).
- Audio: an access unit is decoded and returned once `clock >= pts - lead`; a paused
  (non-advancing) clock returns nothing. A sample-rate change ends the block; the next
  `pull` returns the new-rate samples (carried, never mixed).

## Bounds

- Input: `QueueLimits::VIDEO` (120 AUs / 32 MiB / 16 MiB per AU), `QueueLimits::AUDIO`
  (256 / 4 MiB / 1 MiB). Full queues return `Backpressure`; nothing is dropped silently.
- Output: two RGBA canvases (ahead + published) plus the decoder's DPB; audio output is
  whatever the caller drains per `pull`.
- Shutdown: all work is synchronous inside `feed`/`step`/`pull`; there is no thread or
  child to reap, so drop is bounded by one access unit's decode. An adapter that runs
  `step` on its own thread stops by ceasing calls and dropping the session.

## Output conversion

`scale.rs`: fitted size per `force_original_aspect_ratio=decrease` on the coded display
size, offsets rounded down to even (4:2:0), separable bicubic resampling, YUV→RGBA with
the stream matrix (BT.601 when unspecified) and range, opaque black padding.
Bit-exactness against the reference's swscale path is graded natively; see the D4 status.

`mix.rs`: stereo downmix (front unity, centre/surround −3 dB, LFE omitted, normalised to
avoid overload), round-to-nearest s16. The exact `-ac 2` reference behaviour must be
frozen from native observation before grading (`research/d4/DOWNMIX-CONTRACT.json`).

## Device boundary

Pulse/ALSA output and GL upload stay in the consumer adapter (platform APIs). They are
declared consumer boundaries, not FFmpeg components; no native decoder or FFmpeg process
is involved anywhere in this path.
