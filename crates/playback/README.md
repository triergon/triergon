# triergon-playback

Composes the Matroska, MP4 and MPEG-TS readers into a player-facing packet
feed: caller-owned sources, format probing, bounded container events, track
selection, seeking, packet and subtitle bridges, and a producer/sink pump. It
sits above `triergon` (Matroska), `triergon-isobmff`, `triergon-mpegts`,
`triergon-framing` and `triergon-subtitles`. It does not fetch over HTTP,
parse playlists, retry transports, decode media or render.

## Usage

```rust
let source = SourceOwner::new(ByteSource::new(id, bytes, 64 * 1024)?);
let input = InputCursor::new(&pool, id, caps, 4096)?;
let engine = Engine::ts(input, PacketFormat::Ts188, program, ParseLimits::default(), policy)?;
let mut player = Player::new(engine, source, Selection::All, PlayerPolicy::default())?;
loop {
    match player.advance(&mut WorkBudget::new(4096), &cancel)? {
        Progress::Ended => break,
        Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
        other => handle(other), // Backpressure, Paused, AwaitEntity, ...
    }
    while let Some(out) = player.pop() {
        if player.may_deliver(&out) { deliver(out.event()); }
    }
}
```

Implement `Source` for real transports. `ByteSource` serves immutable local
bytes. Use `io::PlaybackProbe` (or `io::Probe` for containers only) when the
format is unknown; its `PlaybackInput` holds a `SelectedInput` or a
`RawVideoSource` that replays the probed prefix and then continues reading.
MP4 needs an open and index step first: `mp4_open::Mp4Open` →
`mp4_plan::Mp4IndexJob` → `Engine::mp4`.

## Supported

| Area | Entry points |
|---|---|
| Source protocol | `Source`, `SourceReply`, `ByteSource`, `io::SourceOwner` |
| Probe | `io::PlaybackProbe`: Matroska, MP4, TS188, M2TS192, AVC and HEVC Annex B |
| Matroska | `Engine::matroska`, `matroska_with_payload_ownership`, `matroska_with_immutable_seek_index`, `matroska_webm_vp9_t35_v1` |
| Matroska feeds | `matroska_feed::MatroskaFeedSetup` (AVC Annex B, AAC ADTS), `MatroskaHevcDolbySetup` (HEVC Annex B, AC-3/E-AC-3), `MatroskaDiscovery`, `MatroskaCompactDiscovery`, `MatroskaVideoSeekGate` |
| Matroska clocks | feed methods `native_avc_clock`, `native_aac_clock_probe` (`MatroskaTrackFeed`), `native_hevc_clock`, `native_dolby_clock_probe` (`MatroskaHevcDolbyFeed`); `MatroskaBitmapClockProbe` |
| MP4 / MOV | `mp4_open::Mp4Open` (`new`, `new_progressive`, `new_pinned_mov_recovery`), `Engine::mp4`, `Engine::mp4_progressive`, `Engine::mp4_native_terminal_v1`, `Mp4RangeOpen` / `Mp4RangePlayer` |
| MP4 seek | `Player::prepare_mp4_sync_seek` / `apply_mp4_sync_seek`, `prepare_mp4_native_seek` / `apply_mp4_native_seek`, progressive variants, `PreparedMp4::movie_seek` |
| MP4 catalogue replacement | `catalogue::Mp4Catalogue`, `Player::prepare_mp4_catalogue` / `replace_mp4_catalogue` |
| TS / M2TS | `Engine::ts`, `ts_with_namespace`, `ts_segmented`, `ts_native`, `ts_native_private_audio_v1`, `ts_native_program_v1`, `ts_native_segment_v1`; `native_startup::NativeStartup` |
| HLS media segments | `hls_segment::HlsSegmentReader` (AVC + AAC), `HlsMediaSegmentReader` (AVC/HEVC + AAC/AC-3/E-AC-3) |
| Raw Annex B | `raw_video::RawVideoProbe`, `RawVideoSource`, `RawVideoConfigTracker`, `RawVideoFeed` |
| Packet bridges | `nal_bridge::AvcBridge`, `hevc_bridge::HevcBridge`, `aac_bridge::AacBridge`, `av1_bridge::Av1Bridge` |
| Subtitles | `text_bridge::TextBridge`, `bitmap_bridge::{PgsBridge, DvdJob, DvdReceiver}`, `matroska_feed::MatroskaBitmapTrack`, `dvb_ts::DvbTsBridge` |
| Sink handoff | `pump::{Pump, Sink, CatalogueSink, Feed}` |

## Behaviour and limits

**Source protocol.** `Source::poll` must return promptly. `Pending` and empty
data never mean end of stream; only `SourceReply::End` does, and transport
failure is a separate reply. `abort` wakes and quiesces any I/O producer
before `close`. HTTP, authentication and retry policy belong to the caller.

**Bounded work.** Every call takes a `WorkBudget` and a cancel flag, and all
retained memory is charged to a `triergon_core::Budget`. `Backpressure` is
retryable and keeps the pending request, packet and buffers. `PlayerPolicy`
defaults to 16 FIFO slots (at most 64), 32 MiB queued payload, 128 MiB seek
input and 10,000,000 events. `Player::new_direct` skips the FIFO for callers
that use `Player::read_output` with one caller-owned slot.

**Clocks.** Every packet keeps its exact source timestamps. FFmpeg-compatible
packet clocks are explicit opt-in views (`ClockChoice::ExplicitNativeProjection`,
`Mp4View::PinnedMov9NoParseNoFillin`, the `native_*` Matroska clocks and the
`*_native_*` TS engines); they never overwrite source facts. Native views can
emit events the source view does not, such as `Mp4Event::NativeShortPacket`,
`NativeMissingSample` and native refusals.

**Generations.** A seek, track switch or configuration change starts a new
generation. Check outputs with `Player::may_deliver`. Feeds, bridges, clocks
and subtitle receivers are bound to one generation and configuration; rebuild
them after a change. `Delivery::DecodeOnly` marks preroll before the seek
target.

**Matroska.** `SharedBounded` payload ownership lets packets borrow complete
blocks from the source window; a borrowed packet keeps the whole backing
allocation charged until it drops. Blocks split across reads are copied. The
immutable seek index reuses keyframe positions after one sequential read and
needs no Cues. The Dolby clock probe retains at most 4,096 timing rows and
inspects up to 32 syncframes to infer a missing duration.

**MP4.** Progressive playback of a front-`moov` file continues through later
`moof`/`mdat` pairs with at most 64 pending fragment slots. A size-zero
terminal `mdat` ends only on a matching `SourceReply::End`. Forward-only
reordering retains at most 32 KiB of media. An in-band second `moov` is not
accepted; replace the catalogue explicitly with a freshly opened source.
Range seeks plan from the sample tables without reading payload; the first
media request starts at the command's `start_offset()`.

**MPEG-TS.** `ts_native_program_v1` selects up to 64 PIDs of one program.
Private stream type `0x06` is audio only after explicit AC-3/E-AC-3 or DVB
admission. M2TS prefix startup (`NativeStartup::new_m2ts_prefix_v2`) needs an
8 KiB input window. The DVB native PES parser caps a continued segment at
65,536 bytes.

**HLS segments.** One independent TS188 segment with in-band PAT and PMT.
The feed consumes validated PES continuations directly and preserves source
event positions.
Units are released only after EOF and both the video and audio assemblers drain. Defaults: 64 MiB
source, 64 MiB feed, 8,192 access units. A failed reader releases its source;
retry with a new reader.

**Sinks.** `pump::Sink` owns accepted `Feed` envelopes until their callbacks
retire; `Submission::Blocked` returns the same envelope. Threading and
platform decoder configuration are the sink's responsibility; no `Send` bound
is imposed.

## Conformance

- Matroska: [codec mappings](https://www.ietf.org/archive/id/draft-ietf-cellar-codec-21.html).
- MP4: ISO/IEC 14496-12 and the [QuickTime seek tables](https://developer.apple.com/documentation/quicktime-file-format/seeking_with_a_quicktime_file);
  AV1 per the [AV1-ISOBMFF sample format](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html#av1-sample-format).
- MPEG-TS: ITU-T H.222.0 (§2.14.1 AVC delimiters); DVB subtitles per
  [ETSI EN 300 468 §6.2.41](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf).
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

Integration tests in `tests/` replay authored and FFmpeg-generated fixtures
and compare packet order, bytes, flags, PTS, DTS and duration with pinned
FFprobe records. Fixture provenance: [AV1](tests/fixtures/av1/README.md),
[Matroska feeds](tests/fixtures/mkv-feed/README.md),
[Dolby clocks](tests/fixtures/dolby-clock/README.md),
[subtitle probes](tests/fixtures/matroska-subtitle-probe/README.md).
Gates and measurement policy: [docs/quality.md](../../docs/quality.md).
