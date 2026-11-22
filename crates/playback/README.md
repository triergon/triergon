# triergon-playback

Composes the Matroska, MP4 and MPEG-TS readers into a player-facing packet
feed: caller-owned sources, format probing, bounded container events, track
selection, seeking, packet and subtitle bridges, and a producer/sink pump. It
sits above `triergon-matroska`, `triergon-isobmff`, `triergon-mpegts`,
`triergon-framing` and `triergon-subtitles`. It does not fetch over HTTP,
parse playlists, retry transports, decode media or render. It has no Cargo
features.

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{PacketFormat, ParseLimits, clock::SourceTimePolicy};
use triergon_playback::{
    ByteSource, Engine, Player, PlayerPolicy, Progress, Selection, io::SourceOwner,
};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/media-00.ts");
let pool = Budget::new(64 << 20)?;
let id = SourceId(1);
let bytes = Bytes::copy_from_slice(&pool, &std::fs::read(path)?)?;
let caps = SourceCapabilities { length: Some(bytes.len() as u64), random_access: true };
let source = SourceOwner::new(ByteSource::new(id, bytes, 64 * 1024)?); // or your own `Source`
let input = InputCursor::new(&pool, id, caps, 4096)?;
let (program, policy) = (1, SourceTimePolicy::RelativeEpochZero);
let engine = Engine::ts(input, PacketFormat::Ts188, program, ParseLimits::default(), policy)?;
let mut player = Player::new(engine, source, Selection::All, PlayerPolicy::default())?;
let cancel = AtomicBool::new(false);
let mut packets = 0;
loop {
    let progress = player.advance(&mut WorkBudget::new(4096), &cancel)?;
    while let Some(out) = player.pop() {
        if player.may_deliver(&out) && out.event().track().is_some() {
            packets += 1; // deliver(out.event())
        }
    }
    match progress {
        Progress::Ended => break,
        Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
        other => return Err(format!("{other:?}").into()), // Backpressure, Paused, AwaitEntity, ...
    }
}
assert!(packets > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Implement `Source` for real transports; `ByteSource` serves immutable local
bytes. When the format is unknown, `io::PlaybackProbe` (or `io::Probe` for
containers only) yields a `PlaybackInput` that replays the probed prefix. MP4 is
recognised by its opening boxes (`ftyp`, `styp`, `moov`, `free`, `wide`, `mdat`);
a transport stream needs exactly one of the TS188 and M2TS192 three-sync patterns
in a 389-byte prefix; raw Annex B selects AVC or HEVC at the first slice after its
parameter sets.

MP4 needs an open and index step first: `mp4_open::Mp4Open` →
`mp4_plan::Mp4IndexJob` → `Engine::mp4`. `Mp4RangeOpen::for_media` projects
media tracks for playback and seek alignment; data tracks such as RTP hints and
timecode still have their sample extents checked. Supply any edit plans in source
track order, with `None` for each data track (`avc-aac-rtp-hints.mp4` places an RTP
hint track between video and audio).

## Supported

| Area | Entry points |
|---|---|
| Source protocol | `Source`, `SourceReply`, `ByteSource`, `io::SourceOwner` |
| Probe | `io::PlaybackProbe`: Matroska, MP4, TS188, M2TS192, AVC and HEVC Annex B |
| Matroska | `Engine::matroska`, `Engine::matroska_reader` (an unstarted reader built with `Reader::new_with_payload_ownership`, `new_with_immutable_seek_index` or `compat::block_additions::new_reader`) |
| Matroska feeds | `matroska_feed::MatroskaFeedSetup` (AVC Annex B, AAC ADTS), `MatroskaHevcDolbySetup` (HEVC Annex B, AC-3/E-AC-3), `MatroskaDiscovery`, `MatroskaCompactDiscovery`, `MatroskaVideoSeekGate` |
| Matroska clocks | feed methods `native_avc_clock`, `native_aac_clock_probe` (`MatroskaTrackFeed`), `native_hevc_clock`, `native_dolby_clock_probe` (`MatroskaHevcDolbyFeed`); the audio probes are `MatroskaClockProbe`s; `MatroskaBitmapClockProbe` |
| MP4 / MOV | `mp4_open::Mp4Open` (`new`, `new_progressive`, `new_recovery`, `with_timing_policy`), `Engine::mp4`, `Engine::mp4_progressive`, `Engine::mp4_native_terminal_v1`, `Mp4RangeOpen` / `Mp4RangePlayer` |
| MP4 seek | `Player::prepare_mp4_sync_seek` / `apply_mp4_sync_seek`, `prepare_mp4_native_seek` / `apply_mp4_native_seek`, progressive variants, `PreparedMp4::movie_seek` |
| MP4 catalogue replacement | `catalogue::Mp4Catalogue`, `Player::prepare_mp4_catalogue` / `replace_mp4_catalogue` |
| TS / M2TS | `Engine::ts`, `ts_with_namespace`, `ts_segmented`, `ts_native`, `compat::pes::open_private_audio`, `ts_native_program_v1`, `ts_native_segment_v1`; `native_startup::NativeStartup` |
| Progressive TS188 | `ts_progressive::TsProgressiveReader` (AVC + ADTS AAC) |
| HLS media segments | `hls_segment::HlsSegmentReader` (AVC + AAC), `HlsMediaSegmentReader` (AVC/HEVC + AAC/AC-3/E-AC-3) |
| Raw Annex B | `raw_video::RawVideoProbe`, `RawVideoSource`, `RawVideoConfigTracker`, `RawVideoFeed` |
| Packet bridges | `aac_bridge::AacBridge`, `av1_bridge::Av1Bridge` (including MP4 `av01` and `dav1`); MP4 AVC and HEVC samples frame through `triergon_framing::NalPacketSession` |
| Subtitles | `text_bridge::TextBridge`, `bitmap_bridge::{PgsBridge, DvdReceiver}`, `matroska_feed::MatroskaBitmapTrack`, `mp4_dvd::{Mp4DvdSetup, Mp4DvdTrack}`, `dvb_ts::DvbTsBridge`, `pgs_ts::PgsTsTrack`, `teletext_ts::TeletextTsBridge`, `caption_bridge::CaptionBridge` |
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
`NativeEmptyPacket`, `NativeMissingSample` and native refusals.

**Generations.** A seek, track switch or configuration change starts a new
generation. Check outputs with `Player::may_deliver`. Feeds, bridges, clocks
and subtitle receivers are bound to one generation and configuration; rebuild
them after a change. `Delivery::DecodeOnly` marks preroll before the seek
target.

**Matroska.** With `SharedBounded` payload ownership packets borrow complete
blocks from the source window and keep the whole backing allocation charged until
they drop; blocks split across reads are copied. The immutable seek index needs no
Cues. HEVC startup requires one distinct parameter set of each type. Legacy
`A_AAC/MPEG2/LC` and `A_AAC/MPEG4/LC` tracks without an initialization record get
AAC-LC configuration from `compat::matroska::legacy_aac_lc_initialization` for one
to six channels. The Dolby clock probe retains at most 4,096 timing rows and
inspects up to 32 syncframes to infer a missing duration.

**MP4.** Progressive playback of a front-`moov` file continues through later
`moof`/`mdat` pairs in batches bounded by `progressive::ProgressiveLimits` (by default 64
fragments). A size-zero terminal `mdat` ends only on a matching
`SourceReply::End`. Forward-only reordering retains at most 32 KiB of media. An
in-band second `moov` is not accepted; replace the catalogue explicitly with a
freshly opened source. Range seeks plan from the sample tables without reading
payload; the first media request starts at the command's `start_offset()`.
Source sample clocks and byte ranges stay literal while compatibility packet
clocks reflect edits; AAC leading trim is carried in
`Mp4Packet::encoded_skip_samples`. `compat::mov::forward_encoded_edits` refuses
edits that would revisit media bytes. A refused
`Mp4RangePlayer::prepare_seek_with_opus_recovery` leaves the active generation and
ordinary seek available. `subp` tracks are subtitle tracks.

**MP4 recovery view.** The FFmpeg-compatible packet view opens with
`Mp4Open::new_recovery`. When the metadata precedes all media
(`MetadataReadScope::FrontMetadataBeforeMdat`, as a nonseekable source with a
front `moov` gives), it indexes with `Mp4IndexJob::new_native_terminal_v1` and
reads with `Engine::mp4_native_terminal_v1`; otherwise it indexes with
`Mp4View::PinnedMov9NoParseNoFillin` and reads with `Engine::mp4`. A first sample
outside the source ends the view with `NativeMissingSample` and a truncated sample
becomes a corrupt `NativeShortPacket`. `Mp4Open::new` instead refuses
out-of-source, truncated and zero-timescale media and reads zero-size samples as
declared.

**MPEG-TS.** `ts_native_program_v1` selects up to 64 PIDs of one program.
Private stream type `0x06` is audio only after explicit AC-3/E-AC-3 or DVB
admission, and a subtitle track when a DVB namespace names DVB subtitling or
teletext; stream type `0x90` (PGS) is a subtitle track. M2TS prefix startup
(`NativeStartup::new_m2ts_prefix_v2`) needs an 8 KiB input window.
`PreparedNative::into_program_player` selects up to 64 PIDs from the prepared
M2TS source and retains its native clock and original carrier offsets through
physical EOF. The DVB native PES parser caps a continued segment at 65,536 bytes.

**Progressive TS188.** One sequential input with a stable PAT, PMT and clock,
one AVC track and one ADTS AAC track. AVC starts on a timed IDR with an
access-unit delimiter; AAC needs an initial PES timestamp. Start a new reader to
begin at another offset.

**HLS segments.** One independent TS188 segment with in-band PAT and PMT; units
are released only after EOF. `HlsSegmentLimits` defaults: 64 MiB source, 64 MiB
feed, 8,192 access units. A failed reader releases its source; retry with a new
reader.

**Subtitles.** The TS bridges bind to one PID of one active PMT; a new
generation, PMT epoch or clock epoch needs a new bridge. Their times are PES times
on the player's timeline. Teletext pages and closed captions become opaque ASS
`TextSource`s that `TextBridge` reports; an empty teletext page is a clear. A
teletext decode needs the decoder's whole packet work in one budget.
`CaptionBridge` takes H.264 or HEVC access units or MPEG-2 PES payloads in decode
order; build it with the track's `avcC` or `hvcC` record when samples are
length-prefixed, again after a seek, or pictures without parameter sets lose their
captions.

**Sinks.** `pump::Sink` owns accepted `Feed` envelopes until their callbacks
retire; `Submission::Blocked` returns the same envelope. Threading and
platform decoder configuration are the sink's responsibility; no `Send` bound
is imposed.

## Conformance

- Matroska: [codec mappings](https://www.ietf.org/archive/id/draft-ietf-cellar-codec-21.html).
- MP4: ISO/IEC 14496-12 and the [QuickTime seek tables](https://developer.apple.com/documentation/quicktime-file-format/seeking_with_a_quicktime_file);
  AV1 per the [AV1-ISOBMFF sample format](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html#av1-sample-format).
- MPEG-TS: ITU-T H.222.0 (§2.14.1 AVC delimiters); DVB subtitles per
  [ETSI EN 300 468 §6.2.41](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf),
  teletext per ETSI EN 300 472.
- Closed captions: ATSC A/53 Part 4 `cc_data()` in ITU-T T.35 SEI messages.
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

Verification: integration tests in `tests/` compare packet order, bytes, flags,
PTS, DTS and duration with pinned FFprobe records; the harness contracts are under
[harness/features](../../harness/features) and results are in
[docs/status.md](../../docs/status.md).

## Fixtures

Provenance for other fixture sets: [AV1](tests/fixtures/av1/README.md),
[Matroska feeds](tests/fixtures/mkv-feed/README.md),
[Dolby clocks](tests/fixtures/dolby-clock/README.md),
[subtitle probes](tests/fixtures/matroska-subtitle-probe/README.md),
[subtitle bridges](tests/fixtures/subtitles/README.md),
[terminal-media FFprobe records](tests/fixtures/terminal-matrix/README.md).

The MP4 fixtures below use the repository's dual licence. The
[range corpus](../../harness/features/mp4-range/corpus.json) records the SHA-256s of
the `avc-aac-*.mov` files and the
[`mp4-forward` corpus](../../harness/features/mp4-forward/corpus.json) those of the
`forward-*` files.

- `avc-aac-quicktime-*.mov`: six-channel 48 kHz AAC, 44.1 kHz AAC with AVC B
  pictures, a fragmented remux of `fragmented-avc-aac.mp4` with empty initialization
  sample tables, and 7.1 AAC-LC (channel configuration 12, one distinct tone per
  speaker, encoded by FFmpeg 9.0.2).
- `avc-aac-program.mov`: a seven-tone 48 kHz AAC-LC program with a leading edit.
  To regenerate it with FFmpeg 9.0.2 and libx264:

```sh
signal='0.13*sin(2*PI*107*t)|0.13*sin(2*PI*211*t)|0.13*sin(2*PI*307*t)|0.13*sin(2*PI*53*t)|0.13*sin(2*PI*409*t)|0.13*sin(2*PI*509*t)|0.13*sin(2*PI*601*t)'
ffmpeg -nostdin -f lavfi -i 'testsrc2=s=96x64:r=25' \
  -f lavfi -i "aevalsrc=${signal}:s=48000:c=6.1(back)" -t 2.4 \
  -c:v libx264 -threads:v 1 -g 25 -keyint_min 25 -sc_threshold 0 -bf 0 -pix_fmt yuv420p \
  -c:a aac -aac_pce 1 -threads:a 1 -b:a 224k -flags:a +bitexact -fflags +bitexact \
  -video_track_timescale 16000 -movie_timescale 48000 -use_editlist 1 -movflags +faststart \
  avc-aac-program.mov
```

- `forward-aac-priming.mp4`: a 64 × 48 AVC test pattern at 12 fps and a 440 Hz mono
  AAC tone at 44.1 kHz. To reproduce it with FFmpeg 9.0.2 and libx264:

```sh
ffmpeg -nostdin -v error \
  -f lavfi -i 'testsrc2=s=64x48:r=12' -f lavfi -i 'sine=f=440:r=44100' \
  -t 0.35 -map 0:v:0 -map 1:a:0 -c:v libx264 -threads:v 1 -profile:v baseline \
  -pix_fmt yuv420p -g 2 -bf 0 -c:a aac -b:a 40k -movflags +faststart forward-aac-priming.mp4
```

- `forward-aac-tail-trim.mp4` keeps the 44,100 Hz movie timescale and sets the audio
  edit to media time zero and duration 16,459.
- `forward-aac-fractional-edit.mp4` sets the movie timescale to 1,000, its duration
  and the video edit's duration to 334, and the audio edit's media time and duration
  to 64 and 45. The audio edit rounds to an end one sample into the third packet.
- `forward-fragment-aac-edit.mp4` and `forward-fragment-aac-late-track.mp4` use an
  FFmpeg-generated AVC test pattern and mono AAC tone with an open-ended audio
  initialization edit; the latter starts with a video-only fragment.
- `forward-fragment-aac-short.mp4` truncates an authored AVC/AAC fragment inside its
  first AAC packet.
- `forward-fragment-mp3-edit.mp4` uses a generated mono MP3 tone with an
  initialization edit.
