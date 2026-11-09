//! PGS and teletext from TS, DVD subtitles from MP4 and closed captions from
//! MP4 video, each through its playback bridge. Fixtures and expected values:
//! `fixtures/subtitles/README.md`.
mod common;
use common::{collect, off, run_open};
use triergon_core::{
    Budget, Bytes, InputCursor, Result, SourceCapabilities, SourceId, Time, WorkBudget,
};
use triergon_isobmff::{BoxStep, ConfigurationLimits, EntryLayout, Limits};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent,
    clock::SourceTimePolicy,
    psi::{DescriptorNamespace, DvbNamespaceEvidence},
};
use triergon_playback::{
    bitmap_bridge::{DvdReceiver, DvdReceiverStep, PgsBridge, PgsQueueAction, PgsStep},
    caption_bridge::{CaptionBridge, CaptionStep, CaptionUnit, CaptionVideo},
    io::{SelectedInput, SourceOwner},
    mp4_dvd::Mp4DvdSetup,
    mp4_open::{Mp4Open, OpenLimits},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    pgs_ts::{PgsTsStep, PgsTsTrack},
    teletext_ts::{TeletextStep, TeletextTsBridge},
    text_bridge::{TextBridge, TextSource, TextStep},
    *,
};
use triergon_subtitles::cc::video::nal::{Codec, Framing};
use triergon_subtitles::compat::{cc, teletext};
use triergon_subtitles::{pgs, text};

const ID: SourceId = SourceId(77);
const PACKET_LIMIT: usize = 1 << 20;
const PGS_PID: u16 = 0x1200;
const TELETEXT_PID: u16 = 0x100;

fn pool() -> Budget {
    Budget::new(64 << 20).unwrap()
}

fn dvb() -> DescriptorNamespace {
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile)
}

fn ts_player(
    pool: &Budget,
    raw: &[u8],
    format: PacketFormat,
    namespace: DescriptorNamespace,
    selection: Selection,
) -> Player<ByteSource> {
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let input = InputCursor::new(pool, ID, capabilities, 4096).unwrap();
    let engine = Engine::ts_with_namespace(
        input,
        format,
        1,
        ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        namespace,
    )
    .unwrap();
    let source = ByteSource::new(ID, Bytes::copy_from_slice(pool, raw).unwrap(), 512).unwrap();
    Player::new(
        engine,
        SourceOwner::new(source),
        selection,
        PlayerPolicy::default(),
    )
    .unwrap()
}

fn mp4_player(pool: &Budget, raw: &[u8], selection: Selection) -> Result<Player<ByteSource>> {
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let selected = SelectedInput {
        input: InputCursor::new(pool, ID, capabilities, 4096)?,
        source: SourceOwner::new(ByteSource::new(
            ID,
            Bytes::copy_from_slice(pool, raw)?,
            4096,
        )?),
        format: Format::Mp4,
    };
    let mut open = Mp4Open::new(selected, OpenLimits::default())?;
    run_open(&mut open, 64)?;
    let opened = open.finish()?;
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )?;
    while !index.poll(&mut WorkBudget::new(64), &off())? {}
    let engine = Engine::mp4(opened.input, index.finish()?, 1 << 20)?;
    Player::new(engine, opened.source, selection, PlayerPolicy::default())
}

fn subtitle_only(track: TrackKey) -> Selection {
    Selection::Tracks {
        video: None,
        audio: None,
        subtitle: Some(track),
    }
}

fn is_active_map(output: &Output) -> bool {
    matches!(
        output.event(),
        RawEvent::Ts(TsEvent {
            source: TraceEvent::ProgramMap { active: true, .. },
            ..
        })
    )
}

fn mp4_packet(output: &Output) -> Option<&Mp4Packet> {
    match output.event() {
        RawEvent::Mp4(Mp4Event::Packet(packet)) => Some(packet),
        _ => None,
    }
}

/// The cues the text bridge reports for `source`: payload, start and duration.
fn cues(text: &mut TextBridge, source: TextSource) -> Vec<(Vec<u8>, Option<Time>, Option<Time>)> {
    text.provide(source)
        .unwrap_or_else(|e| panic!("{:?}", e.error));
    let mut out = Vec::new();
    loop {
        match text.step(&mut WorkBudget::new(64), &off()).unwrap() {
            TextStep::Yield => {}
            TextStep::NeedPacket => return out,
            TextStep::Event(event) => {
                let text::Event::Cue(cue) = event.event else {
                    panic!("caption metadata")
                };
                assert_eq!(cue.input.format, text::Format::OpaqueAss);
                out.push((cue.input.payload.to_vec(), cue.start, cue.duration));
            }
            TextStep::Finished => panic!("text bridge finished"),
        }
    }
}

fn us(value: i64) -> Option<Time> {
    Some(Time::new(i128::from(value), 1_000_000).unwrap())
}

#[test]
fn ts_pgs_and_teletext_streams_are_subtitle_tracks() {
    let pgs = include_bytes!("fixtures/subtitles/pgs-basic.m2ts");
    let teletext = include_bytes!("fixtures/subtitles/teletext-basic.ts");
    let ts = |program, pid| TrackKey::Ts { program, pid };
    for (raw, format, namespace, pid, admitted) in [
        (
            &pgs[..],
            PacketFormat::M2ts192,
            DescriptorNamespace::Unspecified,
            PGS_PID,
            true,
        ),
        (
            &teletext[..],
            PacketFormat::Ts188,
            dvb(),
            TELETEXT_PID,
            true,
        ),
        // Teletext descriptors name a codec only in a DVB namespace.
        (
            &teletext[..],
            PacketFormat::Ts188,
            DescriptorNamespace::Unspecified,
            TELETEXT_PID,
            false,
        ),
    ] {
        let b = pool();
        let mut player = ts_player(&b, raw, format, namespace, subtitle_only(ts(1, pid)));
        assert_eq!(
            collect(&mut player, 4096).is_ok(),
            admitted,
            "{pid:#x} {namespace:?}"
        );
    }
}

#[test]
fn ts_pgs_reaches_the_pgs_bridge() {
    let b = pool();
    let raw = include_bytes!("fixtures/subtitles/pgs-basic.m2ts");
    let mut player = ts_player(
        &b,
        raw,
        PacketFormat::M2ts192,
        DescriptorNamespace::Unspecified,
        Selection::All,
    );
    let outputs = collect(&mut player, 4096).unwrap();
    let map = outputs
        .iter()
        .find(|o| is_active_map(o))
        .expect("active PMT");
    assert!(PgsTsTrack::new(map, 0x1fff, PACKET_LIMIT).is_err());
    let mut track = PgsTsTrack::new(map, PGS_PID, PACKET_LIMIT).unwrap();
    let mut pgs = PgsBridge::new(
        &b,
        track.scope(),
        pgs::DisplayLimits::default(),
        pgs::DisplayConfig::default(),
    )
    .unwrap();
    let mut actions = Vec::new();
    for output in outputs {
        if output.event().track()
            != Some(TrackKey::Ts {
                program: 1,
                pid: PGS_PID,
            })
        {
            continue;
        }
        let mut mailbox = Some(output);
        assert!(track.push(&mut mailbox).unwrap());
        let input = loop {
            match track.poll(&mut WorkBudget::new(7)).unwrap() {
                PgsTsStep::Yield => {}
                PgsTsStep::Packet(input) => break input,
                PgsTsStep::NeedPacket => panic!("pushed PES lost"),
            }
        };
        pgs.provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        loop {
            match pgs.step(&mut WorkBudget::new(64), &off()).unwrap() {
                PgsStep::Yield => {}
                PgsStep::NeedPacket => break,
                PgsStep::Output(output) => match output.packet_clock_action().unwrap() {
                    Some(PgsQueueAction::Show(picture)) => {
                        let rect = picture.rectangles().next().unwrap();
                        actions.push((
                            picture.start_ns,
                            Some((rect.x, rect.y, rect.width, rect.height)),
                        ));
                    }
                    Some(PgsQueueAction::Clear { at_ns }) => actions.push((at_ns, None)),
                    None => {}
                },
                PgsStep::Finished => panic!("unexpected PGS step"),
            }
        }
    }
    // The reference shows one 400x60 object at (600, 900) at 1 s and clears at 3 s.
    assert_eq!(
        actions,
        [
            (1_000_000_000, Some((600, 900, 400, 60))),
            (3_000_000_000, None)
        ]
    );
}

#[test]
fn teletext_ts_reaches_the_text_bridge() {
    let b = pool();
    let raw = include_bytes!("fixtures/subtitles/teletext-basic.ts");
    let mut player = ts_player(&b, raw, PacketFormat::Ts188, dvb(), Selection::All);
    let outputs = collect(&mut player, 4096).unwrap();
    let map = outputs
        .iter()
        .find(|o| is_active_map(o))
        .expect("active PMT");
    let options = teletext::Options::default();
    let mut bridge = TeletextTsBridge::new(map, TELETEXT_PID, options, PACKET_LIMIT).unwrap();
    let generation = map.generation();
    let mut text = TextBridge::new(&b, ID, 0, generation, text::Limits::default());
    let mut seen = Vec::new();
    let mut step = |bridge: &mut TeletextTsBridge, seen: &mut Vec<_>| loop {
        let mut work = WorkBudget::new(teletext::Decoder::MAX_PACKET_WORK);
        match bridge.poll(&mut work, &off()).unwrap() {
            TeletextStep::Yield => {}
            TeletextStep::NeedPacket => return false,
            TeletextStep::Finished => return true,
            TeletextStep::Clear { at } => seen.push((Vec::new(), at)),
            TeletextStep::Cue(source) => {
                for (line, start, duration) in cues(&mut text, source) {
                    assert_eq!(duration, None);
                    seen.push((line, start));
                }
            }
            TeletextStep::Refused(refusal) => panic!("{refusal:?}"),
        }
    };
    for output in outputs {
        bridge.observe(&output).unwrap();
        if output.event().track()
            != Some(TrackKey::Ts {
                program: 1,
                pid: TELETEXT_PID,
            })
        {
            continue;
        }
        assert!(bridge.push(&mut Some(output)).unwrap());
        assert!(!step(&mut bridge, &mut seen));
    }
    bridge.finish_input();
    assert!(step(&mut bridge, &mut seen));
    // The reference's events: PES times of 10 s on, bounded by the PCR half
    // a second before each.
    let line = |text: &str| text.as_bytes().to_vec();
    assert_eq!(
        seen,
        [
            (line("0,0,Default,,0,0,0,,Good evening."), us(9_640_600)),
            (
                line("1,0,Default,,0,0,0,,This is the news\\Nat nine o'clock."),
                us(11_640_600)
            ),
            (Vec::new(), us(13_640_600)),
            (
                line("2,0,Default,,0,0,0,,\u{2190}MUSIC\u{2192} "),
                us(15_640_600)
            ),
            (
                line("3,0,Default,,0,0,0,,- Where are you going?\\N- Home."),
                us(17_640_600)
            ),
            (Vec::new(), us(19_640_600)),
        ]
    );
}

#[test]
fn mp4_dvd_reaches_the_dvd_receiver() {
    let b = pool();
    let raw = include_bytes!("fixtures/subtitles/dvd-commands.mp4");
    let mut player = mp4_player(&b, raw, Selection::All).unwrap();
    let outputs = collect(&mut player, 4096).unwrap();
    let first = outputs
        .iter()
        .find(|o| mp4_packet(o).is_some_and(|p| p.configuration.track.handler == *b"subp"))
        .expect("subp packet");
    let track_id = mp4_packet(first).unwrap().configuration.track.track_id;
    let mut setup = Mp4DvdSetup::new(first, ConfigurationLimits::default()).unwrap();
    let track = loop {
        if let Some(track) = setup.poll(&mut WorkBudget::new(8)).unwrap() {
            break track;
        }
    };
    assert!(track.config().settings.palette.is_some());
    let mut receiver = DvdReceiver::new(&b, track.scope(), track.config()).unwrap();
    let mut pictures = Vec::new();
    for (sequence, output) in outputs.iter().enumerate() {
        let Some(input) = track.packet(output, sequence as u64).unwrap() else {
            continue;
        };
        // Sample ordinals count from one.
        let index = input.metadata.source.sample.ordinal - 1;
        receiver
            .provide(input)
            .unwrap_or_else(|e| panic!("{:?}", e.error));
        let output = loop {
            match receiver.step(&mut WorkBudget::new(64), &off()).unwrap() {
                DvdReceiverStep::Yield => {}
                DvdReceiverStep::Output(output) => break output,
                _ => panic!("DVD receiver left the packet"),
            }
        };
        let Some(presentation) = output.presentation().unwrap() else {
            continue;
        };
        let p = presentation.picture;
        pictures.push((
            index,
            presentation.show.at,
            (p.start_ms, p.end_ms),
            (p.x, p.y, p.width, p.height),
            p.palette().to_vec(),
        ));
    }
    let grey = vec![0x0000_0000, 0xffef_efef, 0xffcb_cbcb, 0xff97_9797];
    let at = |s: i128| Some(Time::new(s, 1).unwrap());
    // The reference's windows: a subtitle without a stop ends with its packet.
    let shown = |index: u64, window: (u32, u32), xy: (u16, u16), palette: &Vec<u32>| {
        let ms = Time::new(i128::from(window.0), 1000).unwrap();
        (
            index,
            at(index.into()).map(|t| t.checked_add(ms).unwrap()),
            window,
            (xy.0, xy.1, 36, 8),
            palette.clone(),
        )
    };
    assert_eq!(
        pictures,
        [
            shown(0, (0, 2275), (102, 202), &grey),
            shown(
                1,
                (0, 2275),
                (102, 202),
                &vec![0x0030_32f8, 0xff0e_11b8, 0xfff7_3130, 0xffb9_110e]
            ),
            shown(
                2,
                (0, 2275),
                (102, 202),
                &vec![0x0000_0000, 0xffef_efef, 0x44cb_cbcb, 0x8897_9797]
            ),
            shown(3, (0, 2275), (102, 202), &grey),
            shown(5, (0, 2275), (102, 202), &grey),
            shown(7, (0, 2275), (102, 202), &grey),
            shown(8, (1706, 3413), (102, 202), &grey),
            shown(10, (0, 1000), (12, 22), &grey),
            shown(11, (0, 2275), (102, 202), &grey),
            shown(12, (0, 1000), (102, 202), &grey),
            shown(13, (0, 2275), (102, 202), &grey),
        ]
    );
    // A `subp` track can be selected as the subtitle track.
    assert!(mp4_player(&b, raw, subtitle_only(TrackKey::Mp4(track_id))).is_ok());
}

/// The `avcC` NAL unit length size of an MP4 AVC packet's sample entry.
fn avc_framing(packet: &Mp4Packet) -> Framing {
    let mut children = packet
        .configuration
        .description
        .extensions(EntryLayout::Visual, Limits::default())
        .unwrap();
    loop {
        match children.poll(&mut WorkBudget::new(64)).unwrap() {
            BoxStep::Box(child) if child.header().kind == *b"avcC" => {
                let body = &child.bytes()[usize::from(child.header().header_size)..];
                return Framing::Lengths((body[4] & 3) + 1);
            }
            BoxStep::Box(_) | BoxStep::Yield => {}
            BoxStep::End => panic!("no avcC"),
        }
    }
}

#[test]
fn mp4_video_captions_reach_the_text_bridge_in_presentation_order() {
    let b = pool();
    let raw = include_bytes!("fixtures/subtitles/cc-h264-a53.mp4");
    let mut player = mp4_player(&b, raw, Selection::All).unwrap();
    let outputs = collect(&mut player, 4096).unwrap();
    let video: Vec<&Mp4Packet> = outputs
        .iter()
        .filter_map(mp4_packet)
        .filter(|p| p.configuration.track.handler == *b"vide")
        .collect();
    let base = video[0].configuration.track.base.require().unwrap();
    let framing = avc_framing(video[0]);
    let generation = outputs[0].generation();
    let units = video.iter().map(|p| CaptionUnit {
        data: p.source.payload.clone(),
        pts: p.source.sample.cts_ticks.map(|t| i64::try_from(t).unwrap()),
        dts: p.source.sample.dts_ticks.map(|t| i64::try_from(t).unwrap()),
        duration: Some(i64::from(p.source.sample.duration_ticks)),
    });

    // The bridge, one unit at a time under small work budgets.
    let options = cc::Options::default();
    let video_clock = CaptionVideo {
        coding: cc::VideoCoding::Nal(Codec::H264, framing),
        time_base: base,
        max_packet_bytes: PACKET_LIMIT,
    };
    let mut bridge = CaptionBridge::new(&b, ID, generation, video_clock, options).unwrap();
    let mut text = TextBridge::new(&b, ID, 0, generation, text::Limits::default());
    let mut seen = Vec::new();
    let mut drive = |bridge: &mut CaptionBridge, seen: &mut Vec<_>| loop {
        match bridge.poll(&mut WorkBudget::new(97), &off()).unwrap() {
            CaptionStep::Yield => {}
            CaptionStep::NeedUnit => return false,
            CaptionStep::Finished => return true,
            CaptionStep::Cue(source) => seen.extend(cues(&mut text, source)),
        }
    };
    for unit in units {
        bridge.provide(unit).unwrap();
        assert!(!drive(&mut bridge, &mut seen));
    }
    bridge.finish();
    assert!(drive(&mut bridge, &mut seen));

    // The reference's events at 1.4014 s, 3.470133 s and 5.138467 s: the
    // source view keeps the edit list's media time of 3003 ticks at 90 kHz.
    let ms = |value: i128| Some(Time::new(value, 1000).unwrap());
    let times: Vec<_> = seen.iter().map(|(_, start, end)| (*start, *end)).collect();
    assert_eq!(
        times,
        [
            (us(1_434_767), ms(2069)),
            (us(3_503_500), ms(734)),
            (us(5_171_833), ms(567)),
        ]
    );
    assert!(
        seen[0]
            .0
            .starts_with(b"0,0,Default,,0,0,0,,{\\an7}{\\pos(38,213)}\\h\\h\\h\\hPOP-ON")
    );
    assert!(seen[2].0.ends_with(b"MIDDLE CYAN"));
}
