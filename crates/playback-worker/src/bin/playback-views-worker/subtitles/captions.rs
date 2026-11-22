use super::{
    ABSENT, CANCEL, Case, GRANT, PACKET_LIMIT, Records, cue_from, drain_text, micros, next,
};
use playback_worker::runner::Result;
use triergon_bridge::{
    caption::{CaptionBridge, CaptionStep, CaptionUnit, CaptionVideo},
    text::TextBridge,
    ts::teletext::{TeletextStep, TeletextTsBridge},
};
use triergon_core::{Budget, BudgetVec, Bytes, TimeBase, WorkBudget, container::CodecId};
use triergon_isobmff::{Limits, Next, movie::EntryLayout};
use triergon_mpegts::stream::TraceEvent;
use triergon_playback::{
    ByteSource, Event, Player,
    mp4::{Mp4Event, Mp4Packet},
};
use triergon_subtitles::{
    cc::video::nal::{Codec, Framing},
    compat::{cc, teletext},
    text,
};

pub(super) fn teletext(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let (map, pid) = super::bitmap::ts_map(player, case.ordinal, CodecId::DvbTeletext)?;
    let mut receiver = TeletextTsBridge::new(
        &map,
        pid,
        teletext::Options {
            format: teletext::Format::Ass,
            pages: teletext::PageSelection::parse(case.option("txt_page").unwrap_or("*")),
            default_region: case
                .option("txt_default_region")
                .map(str::parse)
                .transpose()?,
            chop_top: case.option("txt_chop_top") != Some("0"),
            chop_spaces: case.option("txt_chop_spaces") != Some("0"),
            duration: case.option("txt_duration").map(str::parse).transpose()?,
        },
        PACKET_LIMIT,
    )?;
    let mut text = TextBridge::new(
        pool,
        map.source_id(),
        0,
        map.generation(),
        text::Limits::default(),
    );
    records.record(10, &[receiver.header().len() as u64], receiver.header());
    let mut drain = |receiver: &mut TeletextTsBridge| -> Result<()> {
        loop {
            match receiver.poll(
                &mut WorkBudget::new(teletext::Decoder::MAX_PACKET_WORK),
                &CANCEL,
            )? {
                TeletextStep::Cue(source) => cue_from(&mut text, source, records)?,
                TeletextStep::Clear { at } => records.record(11, &[micros(at)?, ABSENT], &[]),
                TeletextStep::Refused(error) => {
                    if records.trace {
                        println!("# teletext decoder refused {error:?}");
                    }
                }
                TeletextStep::Yield => {}
                TeletextStep::NeedPacket | TeletextStep::Finished => return Ok(()),
            }
        }
    };
    while let Some(output) = next(player)? {
        receiver.observe(&output)?;
        if super::bitmap::ts_payload(&output, case.program, pid)
            && receiver.push(&mut Some(output))?
        {
            drain(&mut receiver)?;
        }
    }
    receiver.finish_input();
    drain(&mut receiver)?;
    text.finish();
    drain_text(&mut text, records)
}

fn mov_configuration(packet: &Mp4Packet, kind: [u8; 4]) -> Result<Vec<u8>> {
    let mut children = packet
        .configuration
        .description
        .extensions(EntryLayout::Visual, Limits::default())?;
    loop {
        match children.poll(&mut WorkBudget::new(GRANT))? {
            Next::Item(child) if child.header().kind == kind => {
                return Ok(child.bytes()[usize::from(child.header().header_size)..].to_vec());
            }
            Next::Item(_) | Next::Yield => {}
            Next::End => return Err("caption video configuration absent".into()),
        }
    }
}

fn video_codec(name: &[u8]) -> Option<CodecId> {
    match name {
        b"avc1" | b"avc3" | b"V_MPEG4/ISO/AVC" => Some(CodecId::H264),
        b"hvc1" | b"hev1" | b"V_MPEGH/ISO/HEVC" => Some(CodecId::Hevc),
        b"mp2v" | b"V_MPEG2" | b"V_MPEG1" => Some(CodecId::Mpeg2Video),
        _ => None,
    }
}

fn coding(codec: CodecId, configuration: &[u8]) -> Result<cc::VideoCoding> {
    let codec = match codec {
        CodecId::H264 => Codec::H264,
        CodecId::Hevc => Codec::Hevc,
        CodecId::Mpeg2Video => return Ok(cc::VideoCoding::Mpeg2Pes),
        _ => return Err("caption video codec unsupported".into()),
    };
    Ok(cc::VideoCoding::Nal(
        codec,
        Framing::of_configuration(codec, configuration),
    ))
}

struct Captions {
    decoder: CaptionBridge,
    text: TextBridge,
}

impl Captions {
    fn new(
        case: &Case<'_>,
        output: &triergon_playback::Output,
        codec: CodecId,
        time_base: TimeBase,
        configuration: &[u8],
        records: &mut Records,
    ) -> Result<Self> {
        let pool = output.pool();
        let source = output.source_id();
        let generation = output.generation();
        let video = CaptionVideo {
            coding: coding(codec, configuration)?,
            time_base,
            max_packet_bytes: PACKET_LIMIT,
        };
        let decoder = CaptionBridge::new(
            pool,
            source,
            generation,
            video,
            configuration,
            cc::Options {
                real_time: case.option("real_time") == Some("1"),
                real_time_latency_ms: case
                    .option("real_time_latency_msec")
                    .unwrap_or("200")
                    .parse()?,
                data_field: match case.option("data_field") {
                    Some("first") => cc::DataField::First,
                    Some("second") => cc::DataField::Second,
                    _ => cc::DataField::Auto,
                },
            },
        )?;
        records.record(10, &[decoder.header().len() as u64], decoder.header());
        Ok(Self {
            decoder,
            text: TextBridge::new(pool, source, 0, generation, text::Limits::default()),
        })
    }

    fn provide(&mut self, unit: CaptionUnit, records: &mut Records) -> Result<()> {
        self.decoder
            .provide(unit)
            .map_err(|failure| failure.error)?;
        self.drain(records)
    }

    fn drain(&mut self, records: &mut Records) -> Result<()> {
        loop {
            match self.decoder.poll(&mut WorkBudget::new(GRANT), &CANCEL)? {
                CaptionStep::Cue(source) => cue_from(&mut self.text, source, records)?,
                CaptionStep::Yield => {}
                CaptionStep::NeedUnit | CaptionStep::Finished => return Ok(()),
            }
        }
    }

    fn finish(mut self, records: &mut Records) -> Result<()> {
        self.decoder.finish();
        self.drain(records)?;
        self.text.finish();
        drain_text(&mut self.text, records)
    }
}

fn mov(case: &Case<'_>, player: &mut Player<ByteSource>, records: &mut Records) -> Result<()> {
    let mut ids = Vec::new();
    let mut receiver = None;
    while let Some(output) = next(player)? {
        let Event::Mp4(Mp4Event::Packet(packet)) = output.event() else {
            continue;
        };
        let format = packet.configuration.description.format;
        let Some(codec) = video_codec(&format) else {
            continue;
        };
        let id = packet.configuration.track.track_id;
        if !ids.contains(&id) {
            ids.push(id);
        }
        if ids.get(case.ordinal) != Some(&id) {
            continue;
        }
        if receiver.is_none() {
            let configuration = match codec {
                CodecId::H264 => mov_configuration(packet, *b"avcC")?,
                CodecId::Hevc => mov_configuration(packet, *b"hvcC")?,
                _ => Vec::new(),
            };
            receiver = Some(Captions::new(
                case,
                &output,
                codec,
                packet.configuration.track.base.require()?,
                &configuration,
                records,
            )?);
        }
        let sample = &packet.source.sample;
        if let Some(bridge) = &mut receiver {
            bridge.provide(
                CaptionUnit {
                    data: packet.source.payload.clone(),
                    pts: sample.cts_ticks.map(i64::try_from).transpose()?,
                    dts: sample.dts_ticks.map(i64::try_from).transpose()?,
                    duration: Some(i64::from(sample.duration_ticks)),
                },
                records,
            )?;
        }
    }
    receiver.ok_or("MP4 caption video absent")?.finish(records)
}

fn mkv(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let mut receiver = None;
    while let Some(output) = next(player)? {
        let Event::Matroska(triergon_matroska::Event::Packet(packet)) = output.event() else {
            continue;
        };
        let tracks = packet.configuration.tracks();
        let selected = tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| Some((index, video_codec(track.codec.as_bytes())?)))
            .nth(case.ordinal);
        let Some((index, codec)) = selected else {
            continue;
        };
        if packet.packet.track != index {
            continue;
        }
        if receiver.is_none() {
            let init = packet
                .configuration
                .initialization(index)
                .ok_or("caption initialization absent")?;
            receiver = Some(Captions::new(
                case,
                &output,
                codec,
                TimeBase::new(1, 1_000_000_000)?,
                init.data,
                records,
            )?);
        }
        if let Some(bridge) = &mut receiver {
            bridge.provide(
                CaptionUnit {
                    data: Bytes::copy_from_slice(pool, packet.packet.data.as_ref())?,
                    pts: packet.packet.pts_ns,
                    dts: packet.packet.dts_ns,
                    duration: packet.packet.duration_ns.map(i64::try_from).transpose()?,
                },
                records,
            )?;
        }
    }
    receiver
        .ok_or("Matroska caption video absent")?
        .finish(records)
}

fn ts(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let mut selected = None;
    while let Some(output) = next(player)? {
        let Event::Ts(event) = output.event() else {
            continue;
        };
        match &event.source {
            TraceEvent::ProgramMap {
                active: true, map, ..
            } if selected.is_none() => {
                let stream = map
                    .streams
                    .iter()
                    .filter_map(|s| {
                        let codec = s.codec()?;
                        matches!(codec, CodecId::H264 | CodecId::Hevc | CodecId::Mpeg2Video)
                            .then_some((s.pid, codec))
                    })
                    .nth(case.ordinal);
                if let Some((pid, codec)) = stream {
                    selected = Some((
                        pid,
                        Captions::new(
                            case,
                            &output,
                            codec,
                            TimeBase::new(1, 90_000)?,
                            &[],
                            records,
                        )?,
                    ));
                }
            }
            TraceEvent::Pes(pes) => {
                let Some((pid, bridge)) = &mut selected else {
                    continue;
                };
                if pes.pid != *pid {
                    continue;
                }
                let mut data = BudgetVec::new(pool);
                data.try_reserve_exact(pes.payload_len)?;
                for span in pes.payload() {
                    data.try_extend_from_slice(span)?;
                }
                bridge.provide(
                    CaptionUnit {
                        data: data.freeze().map_err(|failure| failure.error)?,
                        pts: event
                            .source_pts
                            .and_then(|stamp| stamp.ticks90)
                            .map(i64::try_from)
                            .transpose()?,
                        dts: event
                            .source_dts
                            .and_then(|stamp| stamp.ticks90)
                            .map(i64::try_from)
                            .transpose()?,
                        duration: None,
                    },
                    records,
                )?;
            }
            _ => {}
        }
    }
    selected.ok_or("TS caption video absent")?.1.finish(records)
}

pub(super) fn closed(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    match case.container {
        super::Container::Mov => mov(case, player, records),
        super::Container::Mkv => mkv(case, pool, player, records),
        super::Container::Ts | super::Container::M2ts => ts(case, pool, player, records),
    }
}
