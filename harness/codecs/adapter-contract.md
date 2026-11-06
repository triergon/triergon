# Session-layer contract for a host adapter

`crates/session` turns compressed access units into timestamped 1920×1080 RGBA pictures and
interleaved stereo s16 PCM. A host adapter owns the clock, packet framing, output
devices and GPU upload; nothing in the crate spawns a process or links a native library.

| Host action | Call |
|---|---|
| Open a stream | `let sel = triergon_sim::select(payload);` then `VideoSession::load` and `AudioSession::load` with decoders from `adapters::video_decoder` / `audio_decoder` (feature `codecs`) |
| Feed video / audio | `feed(au, pts)`; `Err(FeedError::Backpressure)` means keep the access unit and retry after output progresses |
| Advance video | `step(clock_ns)` decodes until one picture waits ahead of the clock and publishes it once `clock >= pts`; returns true on a new picture |
| Show video | `latest()` → `Published { sequence, generation, pts, rgba }` |
| Drain audio | `pull(clock_ns, lead_ns, &mut PcmBlock)` returns units due at `clock − lead`; a paused clock returns nothing |
| Seek | `VideoSession::flush` (keeps the last picture, new generation) and `AudioSession::stop` |
| Stop / unload | `stop()` on both (picture cleared) |
| End of input | `end_of_stream()` on both; the state becomes `Ended` once all output is delivered |

`select` picks HEVC when the lowercased payload contains `h265` or `hevc`, otherwise
H.264; audio is the literal `"audio":"` value `AC3`, `AC3 PLUS`, `AAC` or `DTS`, otherwise
no audio session. A stream the H.264 decoder cannot decode reports `Failed`, never a
black picture.

## States and timing

- `SessionState::{Idle, Running, Starved, Ended, Failed(reason)}`. Starved (waiting for
  input), Ended (decoded end of stream) and Failed (sticky until the next
  `load`/`flush`/`stop`) are distinct. Feeding after `end_of_stream` is `Invalid`.
- Each picture carries the PTS of the access unit that coded it through reordering.
  Unknown PTS publishes immediately in output order.
- A sample-rate change ends the PCM block; the next `pull` returns the new rate.
- `audio::lead_ns` parses the lead in milliseconds (default 120, clamped to 0–2000).

## Bounds

- Input: `QueueLimits::VIDEO` (120 units, 32 MiB, 16 MiB per unit) and
  `QueueLimits::AUDIO` (256 units, 4 MiB, 1 MiB per unit). Nothing is dropped silently.
- Output: two RGBA canvases plus the decoder's DPB.
- All work is synchronous inside `feed`, `step` and `pull`, so dropping a session is
  bounded by one access unit's decode. Scaling and downmix: `scale.rs`, `mix.rs`.
