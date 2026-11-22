# triergon-playback

Bounded Matroska, MP4 and MPEG-TS packet feeds with caller-owned sources,
format detection, track selection, seeking and a sink adapter.

## Usage

Probe the source, open the engine of its format, and drive one loop: every
packet whose payload is one slice arrives as a `Packet`, which implements
`triergon_core::container::PacketInfo`.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::container::PacketInfo;
use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{ParseLimits, clock::SourceTimePolicy, psi::DescriptorNamespace};
use triergon_playback::mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits};
use triergon_playback::probe::{Probe, ProbeProgress};
use triergon_playback::{
    ByteSource, Delivery, Engine, Format, Player, PlayerPolicy, Progress, Selection,
};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mkv/observed-11.mkv");
let pool = Budget::new(64 << 20)?;
let id = SourceId(1);
let bytes = Bytes::copy_from_slice(&pool, &std::fs::read(path)?)?;
let caps = SourceCapabilities { length: Some(bytes.len() as u64), random_access: true };
let source = ByteSource::new(id, bytes, 64 * 1024)?; // or your own `Source`
let cancel = AtomicBool::new(false);
let work = || WorkBudget::new(4096);

let mut probe = Probe::new(&pool, id, caps, 4096, source)?;
while !matches!(probe.advance(&mut work(), &cancel)?, ProbeProgress::Selected(_)) {}
let selected = probe.finish()?;
let (engine, source) = match selected.format {
    Format::Matroska => {
        let reader = triergon_matroska::Reader::new(&pool, id, caps, Default::default(), 4096)?;
        (Engine::matroska(reader), selected.source)
    }
    Format::Mp4 => {
        let mut open = Mp4Open::new(selected, OpenLimits::default())?;
        while !matches!(open.advance(&mut work(), &cancel)?, OpenProgress::Ready) {}
        let opened = open.finish()?;
        let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default())?;
        while !index.poll(&mut work(), &cancel)? {}
        (Engine::mp4(opened.input, index.finish()?, 1 << 20)?, opened.source)
    }
    Format::Ts(format) => {
        let (program, policy) = (1, SourceTimePolicy::RelativeEpochZero);
        let namespace = DescriptorNamespace::Unspecified;
        let limits = ParseLimits::default();
        let engine = Engine::ts(selected.input, format, program, limits, policy, namespace)?;
        (engine, selected.source)
    }
};

let mut player = Player::new_direct(engine, source, Selection::All, PlayerPolicy::default())?;
let (mut slot, mut payload) = (None, 0);
loop {
    let progress = player.read_output(&mut slot, &mut work(), &cancel)?;
    if let Some(output) = slot.take()
        && player.may_deliver(&output)
        && output.delivery() != Delivery::Unselected
        && let Some(packet) = output.packet()
    {
        payload += packet.payload().len(); // track, times and flags: `PacketInfo`
    }
    match progress {
        Progress::Ended => break,
        Progress::Queued | Progress::Yield | Progress::NeedInput(_) => {}
        other => return Err(format!("{other:?}").into()), // Backpressure, Paused, ...
    }
}
assert!(payload > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

**The loop.** `Player::read_output` fills one caller-owned slot (`Player::new_direct`
allocates no queue); `Player::advance` with `Player::pop` is the same delivery
through a bounded FIFO, and `pump::Pump` hands outputs to a `Sink` that owns them
until its callbacks retire. Check each output with `Player::may_deliver` and read
`Output::delivery` before using it.

**Packets and tracks.** `Output::packet` (or `Event::packet`) shows a Matroska
frame or an MP4 sample as a `Packet<'_>`: a borrow, never a copy.
`Engine::track_count` and `Engine::track` show the tracks known so far as
`Track<'_>`, which implements `TrackInfo`: the Matroska reader's tracks, the MP4
plan's tracks, or the streams of the transport stream's active program map. An
MP4 track's codec and configuration record come from its first sample entry,
which `mp4::PreparedMp4::identify_tracks` reads on request; until then an `mp4a`
entry names no codec and no record.
[`triergon-bridge`](../bridge/README.md)'s `access::AccessUnits` frames a track's
packets as the access units its decoder takes.

**Where the packet view ends.** A transport stream delivers source events
(`Event::Ts`: carriers, tables, PES, faults) and, through `compat::pes`, demux
PES (`Event::DemuxPes`). A PES payload is a list of spans and is not a decoder's
access unit, so `Output::packet` is `None` for both; `Event::{track, source_pts,
demux_pts, payload_bytes}` still describe them. Access units of TS media come
from [`triergon-hls`](../hls/README.md) (HLS segments and progressive TS188) or
from `triergon_mpegts::Reader`, and raw Annex B input is
[`triergon-raw-video`](../raw-video/README.md).

**Two clocks.** A packet keeps its source timestamps: `PacketInfo` on a `Packet`
reports them. The demux clock is what FFmpeg's demuxers report for the same
packet: `Packet::demux_times`, `Event::demux_pts`. `Player::set_clock_view` chooses
which of the two times delivery and seeking (`ClockChoice::Source`, the default,
or `ClockChoice::Demux`). MP4 packets carry demux times only on a plan indexed by
`compat::mov`; TS tracks only when selected through `compat::pes`.

**Seeking.** `Player::seek_and_select` seeks to a time on a video track: Matroska
through its Cues, MP4 and TS by replaying from the start. MP4 plans a seek from
its tables first: `Player::prepare_mp4_sync_seek` (source clock) or
`prepare_mp4_demux_seek` (demux clock) give a job to `poll` and a command to
`apply_*`; `mp4::Mp4RangePlayer` plays and seeks a complete file by byte ranges.
A seek starts a new generation: outputs from before it fail `may_deliver`, and
packets before the target arrive as `Delivery::DecodeOnly`.

**Names.** *Source* is what the container carries, unchanged. *Demux* is the
packet view FFmpeg's demuxer gives of the same bytes, reproduced by the
`compat` modules: `compat::mov` (MP4 packet order and edit clocks) and
`compat::pes` (PES of selected PIDs, with demux boundaries and clocks).

`probe::Probe` accepts MP4 opening boxes `ftyp`, `styp`, `moov`, `free`, `wide`
and `mdat`. For TS, exactly one three-sync pattern must match in the
389-byte prefix. `SelectedInput` replays the prefix before reading more.
`triergon_raw_video::PlaybackProbe` also detects AVC and HEVC Annex B.

## Supported

| Area | Entry points |
|---|---|
| Source protocol | `Source`, `SourceReply` (`provide_to` an input), `ByteSource`, `SourceOwner` |
| Neutral views | `Packet` (`PacketInfo`, `demux_times`), `Track` (`TrackInfo`, `key`), `Engine::{track_count, track}`, `mp4::PreparedMp4::identify_tracks` |
| Probe | `probe::Probe`: Matroska, MP4, TS188 and M2TS192 |
| Matroska | `Engine::matroska` (an unstarted reader built with `Reader::new`, `Reader::new_with_payload_ownership`, `new_with_immutable_seek_index` or `compat::block_additions::new_reader`), `Engine::matroska_reader`, `matroska::ReplayOutput` |
| MP4 / MOV | `mp4::Mp4Open` (`new`, `new_progressive`, `new_recovery`, `with_timing_policy`), `mp4::Mp4IndexJob`, `Engine::mp4`, `Engine::mp4_progressive`, `compat::mov::{index, index_front, index_encoded_range, open_front, forward_encoded_edits}`, `mp4::Mp4RangeOpen` / `Mp4RangePlayer` |
| MP4 seek | `Player::prepare_mp4_sync_seek` / `apply_mp4_sync_seek`, `prepare_mp4_demux_seek` / `apply_mp4_demux_seek`, progressive variants, `PreparedMp4::movie_seek` |
| MP4 catalogue replacement | `mp4::Mp4Catalogue`, `Player::prepare_mp4_catalogue` / `replace_mp4_catalogue` |
| TS / M2TS | `Engine::ts`, `Engine::ts_entities`; demux PES of selected PIDs (1 to 0x1ffe, at most 64) or of a segment's own tracks: `compat::pes::{open, open_segment, open_private_audio}`, `compat::pes::startup::Startup` for bound M2TS startup |
| PES feed | `pes_feed`: the feed [`triergon-hls`](../hls/README.md) reads HLS segments and progressive TS188 input from |
| Sink handoff | `pump::{Pump, Sink, CatalogueSink, Feed}` |

Matroska feeds, packet bridges and subtitle receivers over these outputs are in
[`triergon-bridge`](../bridge/README.md).

## Behaviour and limits

**Source protocol.** `Source::poll` returns promptly with data, pending,
failure or definitive EOF. Retry pending and empty replies. `abort` wakes and
quiesces any producer before `close`. Use `ByteSource` for immutable local
bytes. `triergon_core::blocking::{Blocking, Feed}` drive a `ContainerReader` or
`InputCursor` on a thread that can block in `std::io`.

**Bounded work.** Every call takes a `WorkBudget` and a cancel flag, and all
retained memory is charged to a `triergon_core::Budget`. `Backpressure` is
retryable and keeps the pending request, packet and buffers. `PlayerPolicy`
defaults to 16 FIFO slots (at most 64), 32 MiB queued payload, 128 MiB seek
input and 10,000,000 events. `Player::new_direct` skips the FIFO for callers
that use `Player::read_output` with one caller-owned slot. Transport carrier,
section and PES outputs get their pool identity from their retained storage.

**Clocks.** Every packet keeps its exact source timestamps. FFmpeg-compatible
packet clocks are explicit opt-in views (`ClockChoice::Demux`, the MOV demux
plans of `compat::mov`, the demux clocks of the Matroska feeds and the
`compat::pes` TS engines); they never overwrite source facts. Demux views can
emit events the source view does not, such as `Mp4Event::ShortPacket`,
`EmptyPacket`, `MissingSample` and `Event::PesRefusal`.

**Generations.** A seek, track switch or configuration change starts a new
generation. Check outputs with `Player::may_deliver`. `Delivery::DecodeOnly`
marks preroll before the seek target.

**Matroska.** `SharedBounded` payload ownership lets packets borrow complete
blocks from the source window; a borrowed packet keeps the whole backing
allocation charged until it drops. Blocks split across reads are copied. The
packet's configuration supplies its allocation budget to the output.
The immutable seek index reuses keyframe positions after one sequential read and
needs no Cues.

**MP4.** Indexed packets share immutable track metadata. A complete sample in a
shared input window can retain that window as its payload.
When the movie declares its duration, the range player checks source ranges and
packet projections before merging the tracks for delivery.
Regular AVC with a leading empty edit keeps its video start delay in range and
forward playback. A media edit can start at a later sync sample, and a backward
seek accounts for the composition lead. A short edit retains enough of the next
decode interval to complete its displayed pictures. An empty edit does not
produce a packet.
QuickTime range playback reads AAC configuration from version 1 and version 2
sound descriptions with `wave` extensions.
`Mp4RangeOpen::for_media` projects media tracks for playback and seek alignment.
Data tracks such as RTP hints and timecode still have their sample extents checked.
Supply any edit plans in source track order, with `None` for each data track.
Progressive playback of a front-`moov` file continues through later
`moof`/`mdat` pairs with at most 64 pending fragment slots. A size-zero
terminal `mdat` ends only on a matching `SourceReply::End`. Forward-only
reordering retains at most 32 KiB of media. An in-band second `moov` is not
accepted; replace the catalogue explicitly with a freshly opened source.
Range seeks plan from the sample tables without reading payload; the first
media request starts at the command's `start_offset()`.
When the movie header's duration is unknown or exceeds the presentation clock,
range playback finds the latest packet end while checking projections. It scans
in merged order when the seek restarts could reach their count or storage limit.
Fragmented files support an open-ended rate-one media edit. Forward playback
applies it from the front movie, including tracks first seen in later fragments. A
leading empty subtitle edit delays the subtitle packet clocks. When video
seeks before the first sync packet of a secondary fragment track, range
playback replays that track from its first packet. An AAC track with a
single initialization edit carries leading trim in the first packet's
`Mp4Packet::encoded_skip_samples`. A recovered short first packet carries its
leading trim in `Mp4ShortPacket::encoded_skip_samples`. The MPEG-4 object
type identifies AAC. An edited MP3 track has the projected clocks without AAC
trim. Source sample
clocks and byte ranges stay literal while the compatibility packet clocks
reflect the edit.
For regular front-metadata input with one finite media extent,
`compat::mov::forward_encoded_edits` accepts checked encoded edit plans
and delivers projected packets in source byte order. AAC packets include the
decoder priming and end-of-edit trim needed for PCM playback. The index refuses
edits that require revisiting media bytes.
`Mp4RangePlayer::prepare_seek_with_opus_recovery` includes the regular
Opus track's declared preroll. Decode from the reported first sample and
present PCM from the video landing through the track's edited end. A refused
recovery leaves the active playback generation and ordinary seek available.
For a file with surplus timing entries, `Mp4Open::with_timing_policy` can
use the timing prefix covered by the sample sizes for forward playback and
`Mp4RangeOpen`. Each track's table reports the unused timing occurrences. The source playback view
requires exact coverage. `subp` tracks are subtitle tracks.

The MOV demux packet view opens with `Mp4Open::new_recovery`. When
the opener read the metadata before any media
(`MetadataReadScope::FrontMetadataBeforeMdat`, which a nonseekable source with
a front `moov` gives), it indexes with `compat::mov::index_front`
and reads with `compat::mov::open_front`; otherwise it indexes with
`compat::mov::index` and reads with `Engine::mp4`. It recovers
what FFmpeg recovers: a first sample outside the source ends the view with
`MissingSample`, a truncated sample becomes a corrupt
`ShortPacket` carrying the available prefix, a zero media timescale
reads in a 1/1 base, and a zero-size read ends demux delivery, with an
`EmptyPacket` where FFmpeg emits one. On a seekable source the recovery
opener refuses a zero-size fragment sample in the header, as FFmpeg does.
`Mp4Open::new` refuses out-of-source, truncated and zero-timescale media and
reads zero-size samples as declared. When a single track's first chunk lies
beyond the media read before indexing and a later chunk lies before it, a
source of unknown length may still hold that chunk in a later `mdat`: the
view reads on and refuses it as outside the media extent (`Invalid`) at the
physical end, or as an unsupported layout once the source passes its start.

**MPEG-TS.** `compat::pes::open` selects up to 64 PIDs of one TS188 program.
Private stream type `0x06` is audio only after explicit AC-3/E-AC-3 or DVB
admission, and a subtitle track when a DVB namespace names DVB subtitling or
teletext; stream type `0x90` (PGS) is a subtitle track. M2TS prefix startup (`compat::pes::startup::Startup::new_m2ts_prefix`) needs an
8 KiB input window. The DVB demux PES parser caps a continued segment at
65,536 bytes. PES outputs reuse their payload storage's budget handle, which
remains valid while a caller retains an output after closing the player.
Delivery without a seek uses the presence of the projected presentation
timestamp directly.

**Sinks.** `pump::Sink` owns accepted `Feed` envelopes until their callbacks
retire; `Submission::Blocked` returns the same envelope. Threading and
platform decoder configuration are the sink's responsibility; no `Send` bound
is imposed.

## Conformance

- MP4: ISO/IEC 14496-12 and the [QuickTime seek tables](https://developer.apple.com/documentation/quicktime-file-format/seeking_with_a_quicktime_file).
- MPEG-TS: ITU-T H.222.0.
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

Integration tests in `tests/it/` compare packet order, bytes, flags, PTS, DTS and
duration with pinned FFprobe records. The terminal-media matrix covers 39
inputs at up to 13 combinations of seekability, length, read size and work
budget. `Mp4Open::new_recovery` matches all 499 rows (497 packet lists and two
header refusals). `Mp4Open::new` matches 446 rows, with the 53 strict outcomes
pinned separately.

The MP4 range comparison includes two upstream samples, one with an initial
non-sync AVC prefix and one with tracks shorter than the declared movie.
It checks startup and backward seek through EOF for both.

Fixture provenance: [MP4](tests/fixtures/mp4/README.md),
[AV1](tests/fixtures/av1/README.md),
[Matroska feeds](tests/fixtures/mkv-feed/README.md),
[Dolby clocks](tests/fixtures/dolby-clock/README.md),
[subtitle probes](tests/fixtures/matroska-subtitle-probe/README.md),
[subtitle bridges](tests/fixtures/subtitles/README.md),
[terminal-media FFprobe records](tests/fixtures/terminal-matrix/README.md).
The `triergon-hls`, `triergon-bridge` and `triergon-raw-video` tests read their fixtures here too.
Gates and measurement policy: [docs/quality.md](../../docs/quality.md).
