# triergon-playback

Composes the Matroska, MP4 and MPEG-TS readers into a player-facing packet
feed: caller-owned sources, format probing, bounded container events, track
selection, seeking, packet and subtitle bridges, and a producer/sink pump. It
sits above `triergon-matroska`, `triergon-isobmff`, `triergon-mpegts`,
`triergon-framing` and `triergon-subtitles`. It does not fetch over HTTP,
parse playlists, retry transports, decode media or render.

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

Implement `Source` for real transports. `ByteSource` serves immutable local
bytes. Use `io::PlaybackProbe` (or `io::Probe` for containers only) when the
format is unknown; its `PlaybackInput` holds a `SelectedInput` or a
`RawVideoSource` that replays the probed prefix and then continues reading.
MP4 needs an open and index step first: `mp4_open::Mp4Open` →
`mp4_plan::Mp4IndexJob` → `Engine::mp4`.
Indexed MP4 packets share immutable track metadata. A complete sample in a
shared input window can retain that window as its payload.
When the movie declares its duration, the range player checks source ranges and
packet projections before merging the tracks for delivery.
QuickTime range playback reads AAC configuration from version 1 and version 2
sound descriptions with `wave` extensions. The `avc-aac-quicktime-surround.mov`
fixture contains synthetic AVC and six-channel, 48 kHz AAC. The range comparison
also runs with the optional format and marker atoms replaced by free boxes.
`avc-aac-quicktime-reordered.mov` adds 44.1 kHz AAC and AVC with B pictures.
`avc-aac-quicktime-fragmented.mov` exercises open and seek with empty
initialization sample tables and a version 1 sound description. It remuxes the
synthetic `fragmented-avc-aac.mp4` fixture under the same MIT or Apache-2.0 license.
`avc-aac-quicktime-7-1.mov` uses the same synthetic video pattern and distinct
tones at 251, 379, 503, 71, 641, 769, 911 and 1031 Hz for the eight speakers.
FFmpeg 9.0.2 encoded the fixture as 44.1 kHz AAC-LC with channel configuration
12. The fixture is distributed under the repository's dual licence. The
[range corpus](../../harness/features/mp4-range/corpus.json) records its SHA-256.

`avc-aac-program.mov` uses an authored AVC pattern and seven independent tones
in a 48 kHz AAC-LC program with a leading edit. The range corpus compares its
original configuration and every selected packet on open and backward seek.
The fixture uses the repository's dual licence. To regenerate it with FFmpeg
9.0.2 and libx264:

```sh
signal='0.13*sin(2*PI*107*t)|0.13*sin(2*PI*211*t)|0.13*sin(2*PI*307*t)|0.13*sin(2*PI*53*t)|0.13*sin(2*PI*409*t)|0.13*sin(2*PI*509*t)|0.13*sin(2*PI*601*t)'
ffmpeg -nostdin -f lavfi -i 'testsrc2=s=96x64:r=25' \
  -f lavfi -i "aevalsrc=${signal}:s=48000:c=6.1(back)" -t 2.4 \
  -c:v libx264 -threads:v 1 -g 25 -keyint_min 25 -sc_threshold 0 -bf 0 -pix_fmt yuv420p \
  -c:a aac -aac_pce 1 -threads:a 1 -b:a 224k -flags:a +bitexact -fflags +bitexact \
  -video_track_timescale 16000 -movie_timescale 48000 -use_editlist 1 -movflags +faststart \
  avc-aac-program.mov
```

`Mp4RangeOpen::for_media` projects media tracks for playback and seek alignment.
Data tracks such as RTP hints and timecode still have their sample extents checked.
Supply any edit plans in source track order, with `None` for each data track.
The `avc-aac-rtp-hints.mp4` test fixture contains a synthetic AVC test pattern,
AAC sine audio and two RTP hint tracks, with the first hint track between the
video and audio tracks.

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
| TS / M2TS | `Engine::ts`, `ts_with_namespace`, `ts_segmented`, `ts_native`, `ts_native_private_audio_v1`, `ts_native_program_v1`, `ts_native_segment_v1`; `native_startup::NativeStartup` |
| Progressive TS188 | `ts_progressive::TsProgressiveReader` (AVC + ADTS AAC) |
| HLS media segments | `hls_segment::HlsSegmentReader` (AVC + AAC), `HlsMediaSegmentReader` (AVC/HEVC + AAC/AC-3/E-AC-3) |
| Raw Annex B | `raw_video::RawVideoProbe`, `RawVideoSource`, `RawVideoConfigTracker`, `RawVideoFeed` |
| Packet bridges | `aac_bridge::AacBridge`, `av1_bridge::Av1Bridge` (including MP4 `av01` and `dav1`); MP4 AVC and HEVC samples frame through `triergon_framing::NalPacketSession` |
| Subtitles | `text_bridge::TextBridge`, `bitmap_bridge::{PgsBridge, DvdReceiver}`, `matroska_feed::MatroskaBitmapTrack`, `mp4_dvd::{Mp4DvdSetup, Mp4DvdTrack}`, `dvb_ts::DvbTsBridge`, `pgs_ts::PgsTsTrack`, `teletext_ts::TeletextTsBridge`, `caption_bridge::CaptionBridge` |
| Sink handoff | `pump::{Pump, Sink, CatalogueSink, Feed}` |

## Behaviour and limits

**Compact Matroska replay.** `MatroskaReplayPrefix::feed_packet` prepares a
buffered AAC or AVC packet through the same selection and configuration checks
as the live feed. A caller can inspect the prefix before delivering its events
in order.

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
Transport carrier, section and PES outputs use the budget of their retained
storage for pool identity.

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

**Matroska.** `SharedBounded` payload ownership lets packets borrow complete
blocks from the source window; a borrowed packet keeps the whole backing
allocation charged until it drops. Blocks split across reads are copied. The
packet's configuration supplies its allocation budget to the output.
The immutable seek index reuses keyframe positions after one sequential read and
needs no Cues. The Dolby clock probe retains at most 4,096 timing rows and
inspects up to 32 syncframes to infer a missing duration.

**MP4.** Progressive playback of a front-`moov` file continues through later
`moof`/`mdat` pairs with at most 64 pending fragment slots. A size-zero
terminal `mdat` ends only on a matching `SourceReply::End`. Forward-only
reordering retains at most 32 KiB of media. An in-band second `moov` is not
accepted; replace the catalogue explicitly with a freshly opened source.
Range seeks plan from the sample tables without reading payload; the first
media request starts at the command's `start_offset()`.
Range playback derives the duration from packet end times when the movie
header's duration is unknown or exceeds the presentation clock.
Complete fragmented files with an open-ended rate-one initialization edit
apply the same edit clocks on startup and seek. The first AAC packet carries
leading trim in `Mp4Packet::encoded_skip_samples`.
`Mp4RangePlayer::prepare_seek_with_opus_recovery` includes the regular
Opus track's declared preroll. Decode from the reported first sample and
present PCM from the video landing through the track's edited end. A refused
recovery leaves the active playback generation and ordinary seek available.
For a file with surplus timing entries, `Mp4Open::with_timing_policy` can
use the timing prefix covered by the sample sizes for forward playback and
`Mp4RangeOpen`. Each track's table reports the unused timing occurrences. The source playback view
requires exact coverage. `subp` tracks are subtitle tracks.

The FFmpeg-compatible packet view opens with `Mp4Open::new_recovery`. When
the opener read the metadata before any media
(`MetadataReadScope::FrontMetadataBeforeMdat`, which a nonseekable source with
a front `moov` gives), it indexes with `Mp4IndexJob::new_native_terminal_v1`
and reads with `Engine::mp4_native_terminal_v1`; otherwise it indexes with
`Mp4View::PinnedMov9NoParseNoFillin` and reads with `Engine::mp4`. It recovers
what FFmpeg recovers: a first sample outside the source ends the view with
`NativeMissingSample`, a truncated sample becomes a corrupt
`NativeShortPacket` carrying the available prefix, a zero media timescale
reads in a 1/1 base, and a zero-size read ends native delivery, with a
`NativeEmptyPacket` where FFmpeg emits one. On a seekable source the recovery
opener refuses a zero-size fragment sample in the header, as FFmpeg does.
`Mp4Open::new` refuses out-of-source, truncated and zero-timescale media and
reads zero-size samples as declared. When a single track's first chunk lies
beyond the media read before indexing and a later chunk lies before it, a
source of unknown length may still hold that chunk in a later `mdat`: the
view reads on and refuses it as outside the media extent (`Invalid`) at the
physical end, or as an unsupported layout once the source passes its start.

**MPEG-TS.** `ts_native_program_v1` selects up to 64 PIDs of one program.
Private stream type `0x06` is audio only after explicit AC-3/E-AC-3 or DVB
admission, and a subtitle track when a DVB namespace names DVB subtitling or
teletext; stream type `0x90` (PGS) is a subtitle track. M2TS prefix startup (`NativeStartup::new_m2ts_prefix_v2`) needs an
8 KiB input window. The DVB native PES parser caps a continued segment at
65,536 bytes. PES outputs reuse their payload storage's budget handle, which
remains valid while a caller retains an output after closing the player.
Delivery without a seek uses the presence of the projected presentation
timestamp directly.

**Progressive TS188.** One sequential input with a stable PAT, PMT and clock,
one AVC track and one ADTS AAC track. AVC starts on a timed IDR with an
access-unit delimiter. AAC needs an initial PES timestamp, then advances
by the exact ADTS duration when an anchor is absent. Source timestamps stay
available alongside the playback clocks. Completed units can be delivered
before the end of input. Each advance can process several internal steps within
the caller's work budget. A source that spends its remaining work before a
reply is ready leaves the request pending for the next advance. Start a new
reader to begin at another offset.
Input requests prefer a TS188 boundary within the cursor's window limit. Short
replies continue through the ordinary packet carry path.
On 64-bit targets, progressive PES payloads retain one shared backing handle per
admitted carrier run. The AVC and AAC cursors borrow each logical carrier range, preserving its
validation, work and retirement boundaries. Independently retained AAC frames
still keep the full backing allocation charged. Both framers also expose
`push_compact` for explicit `CompactDemuxPacket` delivery alongside ordinary `push`.
Both paths share track, configuration-epoch and source-clock admission checks.

**Subtitles.** The TS bridges bind to one PID of one active PMT and take its
strict private-stream PES; a new generation, PMT epoch or clock epoch needs a
new bridge. Their times are the PES times on the player's timeline; teletext
times are bounded by the program clock as FFmpeg's demuxer bounds them, read
from the transport packets offered to `TeletextTsBridge::observe`.
`PgsTsTrack` hands each PES to `PgsBridge`. `Mp4DvdSetup` reads the colour
table of an `mp4s` entry, and its `Mp4DvdTrack` hands each sample to
`DvdReceiver`; a subpicture without a stop ends with its packet's duration.
A Matroska VobSub track's `vobsub_settings` reads its palette and canvas; the
DVD queue places pictures on that canvas.
Teletext pages and closed captions become opaque ASS `TextSource`s, which
`TextBridge` reports as it reports a Matroska ASS track's cues; an empty
teletext page is a clear. A teletext decode needs the decoder's whole packet
work in one budget. `CaptionBridge` takes H.264 or HEVC access units or MPEG-2
PES payloads in decode order and decodes the caption frames FFmpeg's
extraction settles, in its order. It charges one unit per packet byte and,
per frame, the receiver's packet work for its size, each paid over as many
polls as needed.

**HLS segments.** One independent TS188 segment with in-band PAT and PMT.
The feed consumes validated PES continuations directly and preserves source
event positions. AVC fixed-width fields consume buffered RBSP bits in byte-sized
groups while retaining emulation-prevention validation. The segment reader uses one caller-owned player output slot.
`HlsMediaVideoUnit::feed_spans` borrows the complete AVC or HEVC Annex B
feed, including any inserted parameter sets. Its spans remain valid while the
video unit is retained.
AVC delimiter lookbehind uses compact indices into four retained PES clocks.
Repeated AVC parameter sets reuse their parsed values when their bytes exactly
match the last stored set. Different bytes follow normal parsing and ID lookup.
Units are released only after EOF and both the video and audio assemblers drain. Defaults: 64 MiB
source, 64 MiB feed, 8,192 access units. A failed reader releases its source;
retry with a new reader.

**ADTS AAC.** Complete frames contained in one source span can share its charged
backing storage when its capacity fits the configured maximum frame size. After
header validation, a fragmented frame can use one copy of the remaining PES tail
when that tail fits the frame size limit (at most 8,191 bytes) and the work budget
covers validation, copying, allocation and span retirement. Every remaining span
must use the same pool and their lengths must exactly match the declared extent.
Frames then share the joined storage and keep its full capacity charged while
retained. Insufficient work, invalid tail metadata or allocation pressure keeps
ordered span assembly, so a later invalid span still follows earlier valid frames.

**Sinks.** `pump::Sink` owns accepted `Feed` envelopes until their callbacks
retire; `Submission::Blocked` returns the same envelope. Threading and
platform decoder configuration are the sink's responsibility; no `Send` bound
is imposed.

`cargo xtask progressive` checks three comparable AVC/AAC TS188 packet folds and
all five authored functional controls through the bounded sequential reader.
All ten literal worker outcomes are required, including positive Rust output and
the recorded reference refusal on the legal duplicate control. The
[contract](../../harness/features/progressive-ts188/contract.json) preserves complete
packet fields and payloads with two retained units per stream. `--benchmark --memory`
uses the standard paired harness for the three original comparable cases only;
native gate-build qualification remains pending.

## Conformance

- Matroska: [codec mappings](https://www.ietf.org/archive/id/draft-ietf-cellar-codec-21.html).
- MP4: ISO/IEC 14496-12 and the [QuickTime seek tables](https://developer.apple.com/documentation/quicktime-file-format/seeking_with_a_quicktime_file);
  AV1 per the [AV1-ISOBMFF sample format](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html#av1-sample-format).
- MPEG-TS: ITU-T H.222.0 (§2.14.1 AVC delimiters); DVB subtitles per
  [ETSI EN 300 468 §6.2.41](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf),
  teletext per ETSI EN 300 472.
- Closed captions: ATSC A/53 Part 4 `cc_data()` in ITU-T T.35 SEI messages.
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

Integration tests in `tests/` replay authored and FFmpeg-generated fixtures
and compare packet order, bytes, flags, PTS, DTS and duration with pinned
FFprobe records. `tests/native_terminal_matrix.rs` reads 39 terminal-media
inputs through the native packet view in 13 source geometries: 12
nonseekable (known and unknown length, read sizes 1, 7 and 4096, work quanta
1 and 4096) for every input, and one seekable read of known length for each
of the 31 original inputs, 499 rows in all, run in debug and release. It compares every packet object
with FFprobe 9.0.2, and where FFprobe refused the header it requires the run
to refuse while opening, before any packet. With `Mp4Open::new_recovery`
499/499 rows pass: 497 identical packet lists and 2 matching header refusals.
With `Mp4Open::new` 446/499 pass; the test pins the other 53 outcomes.
Fixture provenance: [AV1](tests/fixtures/av1/README.md),
[Matroska feeds](tests/fixtures/mkv-feed/README.md),
[Dolby clocks](tests/fixtures/dolby-clock/README.md),
[subtitle probes](tests/fixtures/matroska-subtitle-probe/README.md),
[subtitle bridges](tests/fixtures/subtitles/README.md),
[terminal-media FFprobe records](tests/fixtures/terminal-matrix/README.md).
Gates and measurement policy: [docs/quality.md](../../docs/quality.md).
