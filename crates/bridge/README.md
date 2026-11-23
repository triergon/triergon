# triergon-bridge

Frames packets for decoders, discovers Matroska startup configuration and
demux clocks, and binds [`triergon-playback`](../playback/README.md) outputs to
subtitle receivers.

| Area | Entry points |
|---|---|
| Access units | `access::AccessUnits` from a container track's `TrackInfo` and its packets' `PacketInfo`: AVC and HEVC as Annex B, AAC with an AudioSpecificConfig as ADTS, other codecs and tracks without a record as they are; `for_matroska` for a Matroska track entry, `with_record` for a record that comes from elsewhere |
| MP4 packets | `aac::AacBridge` (MP4 AAC as ADTS, bound to one plan's track and configuration revision), `av1::Av1Bridge` (MP4 `av01` and `dav1`, Matroska `V_AV1`, as low-overhead temporal units) |
| Matroska feeds | `matroska::MatroskaFeedSetup` (AVC Annex B, AAC ADTS), `MatroskaHevcDolbySetup` (HEVC Annex B, AC-3 and E-AC-3), `MatroskaDiscovery`, `MatroskaCompactDiscovery`, `MatroskaVideoSeekGate` |
| Matroska clocks | feed methods `demux_avc_clock`, `demux_aac_clock_probe` (`MatroskaTrackFeed`), `demux_hevc_clock`, `demux_dolby_clock_probe` (`MatroskaHevcDolbyFeed`); the audio probes are `MatroskaClockProbe`s |
| Subtitles | `subtitles::text::TextBridge`, `subtitles::caption::CaptionBridge`, `subtitles::bitmap::{PgsBridge, DvdReceiver}`, `subtitles::matroska::{MatroskaBitmapTrack, MatroskaBitmapClockProbe}`, `subtitles::mp4_dvd::{Mp4DvdSetup, Mp4DvdTrack}`, `subtitles::ts::{dvb::DvbTsBridge, pgs::PgsTsTrack, teletext::TeletextTsBridge}` |

## Access units

Feed `Output::packet` from Matroska or MP4 into `AccessUnits`. The access-unit
readers in `triergon-hls`, `triergon-raw-video` and `triergon-mpegts` already
frame Annex B and ADTS. For a decoder that takes contiguous input, join a sliced
unit through `contiguous_feed` or `segments`. `Event::DemuxPes` and PES carried
in `Event::Ts` have no packet view. `AccessUnits::for_track` refuses Matroska
AAC without CodecPrivate; `AccessUnits::for_matroska` frames it from the
Matroska track entry.

```rust
use std::sync::atomic::AtomicBool;
use triergon_bridge::access::AccessUnits;
use triergon_core::container::{PacketInfo, TrackInfo};
use triergon_core::{Budget, Error, Result, WorkBudget};
use triergon_framing::BuildStep;

fn frame_one(pool: &Budget, track: &dyn TrackInfo, packet: &impl PacketInfo) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let mut setup = AccessUnits::for_track(pool, track, Default::default())?;
    let mut framing = loop {
        match setup.step(&mut WorkBudget::new(16_384), &cancel)? {
            BuildStep::Ready(framing) => break framing,
            BuildStep::Yield => continue,
            BuildStep::Complete => return Err(Error::Closed),
        }
    };
    let mut job = framing.packet(packet)?;
    loop {
        match job.step(&mut WorkBudget::new(16_384), &cancel)? {
            BuildStep::Ready(unit) => {
                // Submit unit.payload() to the decoder before releasing the unit.
                let _ = unit.payload();
                return Ok(());
            }
            BuildStep::Yield => continue,
            BuildStep::Complete => return Err(Error::Closed),
        }
    }
}
```

**ADTS program configuration.** An ADTS header has no room for an
AudioSpecificConfig. For channel configuration 0, `AccessUnits` and `AacBridge`
open the raw data block of the first frame they publish with the configuration's
program config element, as ISO/IEC 14496-3 1.A.4.3 asks, unless the packet's
block already opens with one. A job dropped before it publishes leaves the element
to the next; a framing made afresh, after a seek for example, sends it again.

**Superseded Matroska AAC IDs.** Discovery, `MatroskaFeedSetup` and
`AccessUnits::for_matroska` accept `A_AAC/MPEG2/LC` and `A_AAC/MPEG4/LC`, which
the codec mappings supersede by `A_AAC` (sections 3.4.2 to 3.4.10). They carry
no initialization record (a zero-length CodecPrivate is none), so the ADTS
parameters come from the Audio element's indexed sampling frequency and a count
of one to six channels; a present Audio element can use the EBML defaults for
its children, and larger layouts need an `A_AAC` record. The feed keeps the
source packet bytes, and its demux clock leaves a missing duration unknown.
Discovery replays packets in source order with the original signed Block time and
exact nanosecond CodecDelay.

**Compact Matroska replay.** Inspect the replay prefix before delivering its
events in order. `MatroskaReplayPrefix::feed_packet` checks track selection and
configuration for buffered AAC or AVC.

**Matroska HEVC discovery.** Startup requires one distinct parameter set of
each type. It accepts byte-identical repeats in CodecPrivate and before the
first slice of an access unit. Configuration array types must match parameter
set NAL headers. Repeated VPS timing requires positive tick units and time
scale when those fields are present. The single-layer VPS is checked through
its trailing bits, the SPS through its timing fields, and the base PPS through
its extension flags. The demux packet clock also accepts matching in-band
sets before a picture's first slice.

**Matroska clocks.** The Dolby clock probe retains at most 4,096 timing rows and
inspects up to 32 syncframes to infer a missing duration. Demux clock errors
identify a wrong packet codec, a mismatched feed, or a video sample that cannot
extend the decode clock.
Discovery track and packet scan counts fail with `Error::Policy` at their bounds.

## Subtitles

Keep one `TextBridge` for the selected track and generation. When the bridge
requests a packet, hand over the corresponding playback output and step until
it requests the next one. Call `finish` after the last packet, then step until
`TextStep::Finished`.

```rust
use std::sync::atomic::AtomicBool;
use triergon_bridge::subtitles::text::{TextBridge, TextSource, TextStep};
use triergon_core::{Result, WorkBudget};
use triergon_playback::Output;

fn submit_text(bridge: &mut TextBridge, packet: Output) -> Result<()> {
    let source = TextSource::from_output(packet).map_err(|rejected| rejected.error)?;
    bridge.provide(source).map_err(|rejected| rejected.error)?;
    let cancel = AtomicBool::new(false);
    loop {
        match bridge.step(&mut WorkBudget::new(16_384), &cancel)? {
            TextStep::Cue(output) => {
                // Pass the cue to the subtitle renderer.
                let _ = output.cue;
            }
            TextStep::Refused(refused) => return Err(refused.error),
            TextStep::NeedPacket | TextStep::Finished => return Ok(()),
            TextStep::Yield => continue,
        }
    }
}
```

**Refusals.** A subtitle packet that cannot be read costs only its own output:
`TextStep::Refused` returns a text packet or document whose bytes break its
format or exceed a limit, with the reason; a PES the player discards (an
unbounded one, for example) reaches a TS bridge as the fault the player reports
in its place (`Discarded`); and a packet the DVB decoder refuses as malformed is
`Refused`. Each bridge then takes the next. A feature `TextBridge` does not read
(`Error::Unsupported`) ends the track.

**Text.** `TextBridge` reports every tx3g sample, an empty one included, as a
cue. Matroska `S_TEXT/UTF8` and `S_TEXT/WEBVTT` Blocks are read as strict UTF-8.
`S_TEXT/ASS` and `S_TEXT/SSA` Blocks keep every byte and have no character view
(`Format::OpaqueAss`): the codec mappings have muxers write them in UTF-8
(section 5.3), and an ASS renderer such as `triergon-ass` reads the event line
and decodes its text itself. Teletext pages and closed captions become opaque
ASS `TextSource`s, which `TextBridge` reports as it reports a Matroska ASS
track's cues; an empty teletext page is a clear.

**Transport streams.** The TS bridges bind to one PID of one active PMT and
take its strict private-stream PES; a new generation, PMT epoch or clock epoch
needs a new bridge. Their times are the PES times on the player's timeline;
teletext times are bounded by the program clock as FFmpeg's demuxer bounds
them, read from the transport packets offered to `TeletextTsBridge::observe`.
`DvbTsBridge::new` binds a PID whose PMT entry names one subtitling service,
whatever other descriptors it carries; `new_for_service` selects one of
several. Either keeps the service's composition and ancillary pages.
`PgsTsTrack` hands each PES to `PgsBridge`. A teletext decode needs the
decoder's whole packet work in one budget.

**Bitmaps.** The DVB queue exposes up to eight rectangles per picture in decoder
order and uses zero before it has a presentation clock. `Mp4DvdSetup` reads the
colour table of an `mp4s` entry, and its `Mp4DvdTrack` hands each sample to
`DvdReceiver`; a subpicture without a stop ends with its packet's duration. A
Matroska VobSub track's `vobsub_settings` reads its palette and canvas, or gives
the receiver's defaults for a track without CodecPrivate; the DVD queue places
pictures on that canvas.

**Captions.** `CaptionBridge` takes H.264 or HEVC access units or MPEG-2 PES
payloads in decode order and decodes the caption frames FFmpeg's extraction
settles, in its order. It is made with the track's `avcC` or `hvcC` record when
its samples are length-prefixed, again after a seek: samples often carry no
parameter sets, and pictures without them lose their captions. It charges one
unit per packet byte and, per frame, the receiver's packet work for its size,
each paid over as many polls as needed.

**Generations.** Feeds, bridges, clocks and subtitle receivers are bound to one
playback generation and configuration; rebuild them after a seek, track switch
or configuration change.

## Conformance

- Matroska: [codec mappings](https://www.ietf.org/archive/id/draft-ietf-cellar-codec-21.html).
- AAC in ADTS: ISO/IEC 14496-3 1.A.
- AV1 per the [AV1-ISOBMFF sample format](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html#av1-sample-format).
- DVB subtitles per
  [ETSI EN 300 468 §6.2.41](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf),
  teletext per ETSI EN 300 472.
- Closed captions: ATSC A/53 Part 4 `cc_data()` in ITU-T T.35 SEI messages.

The `bridge-*` feature contracts under `harness/features/` compare each bridge
with FFmpeg or an independent oracle; [status](../../docs/status.md) has the
results and the declared differences. The integration tests (`tests/it`) read
the fixtures of `crates/playback/tests/fixtures`. Fixture provenance:
[AV1](../playback/tests/fixtures/av1/README.md),
[Matroska feeds](../playback/tests/fixtures/mkv-feed/README.md),
[Dolby clocks](../playback/tests/fixtures/dolby-clock/README.md),
[subtitle probes](../playback/tests/fixtures/matroska-subtitle-probe/README.md),
[subtitle bridges](../playback/tests/fixtures/subtitles/README.md).
