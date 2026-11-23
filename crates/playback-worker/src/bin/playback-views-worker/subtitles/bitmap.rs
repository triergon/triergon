use super::{ABSENT, CANCEL, Case, Codec, GRANT, PACKET_LIMIT, Records, next};
use playback_worker::runner::Result;
use triergon_bridge::subtitles::{
    bitmap::{
        BitmapInput, DvdBridgeConfig, DvdReceiver, DvdReceiverStep, PgsBridge, PgsOutput,
        PgsQueueAction, PgsStep,
    },
    matroska::{MatroskaBitmapClockProbe, MatroskaBitmapTrack, MatroskaDvbPacket, MatroskaDvbStep},
    mp4_dvd::Mp4DvdSetup,
    ts::{
        dvb::{DvbQueueAction, DvbServiceSelection, DvbStep, DvbTsBridge},
        pgs::{PgsTsStep, PgsTsTrack},
    },
};
use triergon_core::{Budget, Error, Rounding, TimeBase, WorkBudget, container::CodecId};
use triergon_isobmff::config::ConfigurationLimits;
use triergon_mpegts::{
    psi::stream_type,
    stream::{FaultKind, TraceEvent},
};
use triergon_playback::{ByteSource, Event, Output, Player, TrackKey, TsEvent, mp4::Mp4Event};
use triergon_subtitles::{
    compat::{dvb::ReceiverDecoder, dvd::Limits as DvdLimits},
    pgs::{DisplayConfig, DisplayLimits, PacketClock},
};

fn clock(clock: PacketClock, pts: Option<i64>) -> Result<u64> {
    Ok(pts
        .map(|ticks| {
            clock
                .timebase
                .rescale(ticks, TimeBase::new(1, 1_000_000)?, Rounding::NearestAway)
        })
        .transpose()?
        .map_or(ABSENT, |value| value as u64))
}

fn picture(records: &mut Records, pts: u64, start: u64, end: u64, rectangles: usize) {
    records.record(4, &[pts, start, end, rectangles as u64], &[]);
}

fn rectangle(
    records: &mut Records,
    geometry: [i64; 4],
    forced: bool,
    palette: &[u32],
    rows: impl IntoIterator<Item = impl AsRef<[u8]>>,
) {
    let [x, y, width, height] = geometry;
    records.record(
        5,
        &[
            x as u64,
            y as u64,
            width as u64,
            height as u64,
            u64::from(forced),
            palette.len() as u64,
        ],
        &[],
    );
    for &color in palette {
        records.record(6, &[u64::from(color)], &[]);
    }
    for row in rows {
        records.record(8, &[row.as_ref().len() as u64], row.as_ref());
    }
}

fn pgs_receiver<M>(
    case: &Case<'_>,
    pool: &Budget,
    scope: triergon_bridge::subtitles::bitmap::BitmapScope,
) -> Result<PgsBridge<M>> {
    Ok(PgsBridge::new(
        pool,
        scope,
        DisplayLimits::default(),
        DisplayConfig {
            receiver_forced_only: case.option("forced_subs_only") == Some("1"),
        },
    )?)
}

fn reset_input<M>(input: &BitmapInput<M>) -> BitmapInput<()> {
    BitmapInput {
        scope: input.scope,
        sequence: input.sequence,
        raw: input.raw.clone(),
        clock: input.clock,
        duration: input.duration,
        metadata: (),
    }
}

fn check_pgs_reset<M>(case: &Case<'_>, pool: &Budget, input: &BitmapInput<M>) -> Result<()> {
    let old = input.scope;
    let mut new = old;
    new.generation = old.generation.checked_add(1).ok_or(Error::Overflow)?;
    let mut receiver = pgs_receiver(case, pool, old)?;
    receiver
        .provide(reset_input(input))
        .map_err(|failure| failure.error)?;
    if receiver.reset(new, &mut WorkBudget::new(0), &CANCEL)?
        || !receiver.reset(new, &mut WorkBudget::new(GRANT), &CANCEL)?
    {
        return Err("PGS receiver did not respect the reset work grant".into());
    }
    let refusal = receiver
        .provide(reset_input(input))
        .err()
        .ok_or("PGS receiver accepted a previous-generation packet")?;
    if refusal.error
        != (Error::StaleGeneration {
            expected: new.generation,
            received: old.generation,
        })
        || refusal.input.raw.as_ref() != input.raw.as_ref()
        || refusal.input.scope != old
        || !matches!(
            receiver.step(&mut WorkBudget::new(GRANT), &CANCEL)?,
            PgsStep::NeedPacket
        )
    {
        return Err(
            "PGS reset did not discard a pending packet and preserve the refused input".into(),
        );
    }
    let mut current = refusal.input;
    current.scope = new;
    receiver.provide(current).map_err(|failure| failure.error)?;
    Ok(())
}

fn check_dvd_reset<M>(
    pool: &Budget,
    config: DvdBridgeConfig,
    input: &BitmapInput<M>,
) -> Result<()> {
    let old = input.scope;
    let mut new = old;
    new.generation = old.generation.checked_add(1).ok_or(Error::Overflow)?;
    let mut receiver = DvdReceiver::new(pool, old, config.clone())?;
    receiver
        .provide(reset_input(input))
        .map_err(|failure| failure.error)?;
    if receiver.reset(new, config.clone(), &mut WorkBudget::new(0), &CANCEL)?
        || !receiver.reset(new, config, &mut WorkBudget::new(GRANT), &CANCEL)?
    {
        return Err("DVD receiver did not respect the reset work grant".into());
    }
    let refusal = receiver
        .provide(reset_input(input))
        .err()
        .ok_or("DVD receiver accepted a previous-generation packet")?;
    if refusal.error
        != (Error::StaleGeneration {
            expected: new.generation,
            received: old.generation,
        })
        || refusal.input.raw.as_ref() != input.raw.as_ref()
        || refusal.input.scope != old
        || !matches!(
            receiver.step(&mut WorkBudget::new(GRANT), &CANCEL)?,
            DvdReceiverStep::NeedPacket
        )
    {
        return Err(
            "DVD reset did not discard a pending packet and preserve the refused input".into(),
        );
    }
    let mut current = refusal.input;
    current.scope = new;
    receiver.provide(current).map_err(|failure| failure.error)?;
    Ok(())
}

fn pgs<M>(bridge: &mut PgsBridge<M>, input: BitmapInput<M>, records: &mut Records) -> Result<()> {
    bridge.provide(input).map_err(|failure| failure.error)?;
    drain_pgs(bridge, records)
}

fn projected_pgs<M>(
    bridge: &mut PgsBridge<M>,
    input: BitmapInput<M>,
    records: &mut Records,
) -> Result<()> {
    records.record(
        7,
        &[
            clock(input.clock, input.clock.pts)?,
            clock(input.clock, input.duration)?,
        ],
        &input.raw,
    );
    pgs(bridge, input, records)
}

fn drain_pgs<M>(bridge: &mut PgsBridge<M>, records: &mut Records) -> Result<()> {
    loop {
        match bridge.step(&mut WorkBudget::new(GRANT), &CANCEL)? {
            PgsStep::Output(output) => {
                let queue_start = checked_pgs_queue(&output)?;
                if records.trace {
                    println!("# PGS queue {queue_start:?}");
                }
                if let Some(event) = output.decoded.receiver_output() {
                    picture(
                        records,
                        event.pts_microseconds.map_or(ABSENT, |pts| pts as u64),
                        0,
                        ABSENT,
                        event.rectangles().len(),
                    );
                    for rect in event.rectangles() {
                        let stride = usize::from(rect.stride);
                        let width = usize::from(rect.width);
                        let pixels = rect.pixels().as_ref();
                        let rows = (0..usize::from(rect.height))
                            .filter_map(|row| pixels.get(row * stride..row * stride + width));
                        rectangle(
                            records,
                            [
                                rect.x.into(),
                                rect.y.into(),
                                rect.width.into(),
                                rect.height.into(),
                            ],
                            rect.forced,
                            rect.palette(),
                            rows,
                        );
                    }
                }
            }
            PgsStep::Yield => {}
            PgsStep::NeedPacket | PgsStep::Finished => return Ok(()),
        }
    }
}

fn checked_pgs_queue<M>(output: &PgsOutput<M>) -> Result<Option<i64>> {
    let action = output.packet_clock_action()?;
    let Some(event) = output.decoded.receiver_output() else {
        return if action.is_none() {
            Ok(None)
        } else {
            Err("PGS queue received an action without a decoder event".into())
        };
    };
    let clock = output.packet.input().clock;
    let stamp = clock
        .pts
        .into_iter()
        .chain(clock.dts)
        .find(|&value| value > i64::MIN);
    let timestamp = stamp.map_or(0, |value| {
        let numerator = i128::from(value) * i128::from(clock.timebase.numerator());
        let denominator = i128::from(clock.timebase.denominator());
        let whole = numerator / denominator;
        let fraction =
            ((numerator % denominator).abs() * 1_000_000_000 + denominator / 2) / denominator;
        whole
            .checked_mul(1_000_000_000)
            .and_then(|ns| ns.checked_add(numerator.signum() * fraction))
            .and_then(|ns| i64::try_from(ns).ok())
            .unwrap_or(i64::MIN)
    });
    let rectangles = event.rectangles();
    let displayable: Vec<_> = rectangles
        .iter()
        .take(8)
        .filter(|rect| {
            let (width, height, stride) = (rect.width, rect.height, rect.stride);
            (1..=8192).contains(&width)
                && (1..=8192).contains(&height)
                && stride >= width
                && rect.pixels().len()
                    >= usize::from(stride) * (usize::from(height) - 1) + usize::from(width)
        })
        .collect();
    match action {
        None if !rectangles.is_empty() && displayable.is_empty() => Ok(None),
        Some(PgsQueueAction::Clear { at_ns }) if rectangles.is_empty() && at_ns == timestamp => {
            Ok(Some(at_ns))
        }
        Some(PgsQueueAction::Show(picture)) if !displayable.is_empty() => {
            let dimensions = output.decoded.context_canvas;
            let canvas = dimensions
                .iter()
                .all(|side| (160..=8192).contains(side))
                .then_some(dimensions);
            let observed: Vec<_> = picture.rectangles().collect();
            if picture.start_ns != timestamp
                || picture.canvas != canvas
                || observed.len() != displayable.len()
                || !observed
                    .into_iter()
                    .zip(displayable)
                    .all(|(actual, expected)| std::ptr::eq(actual, expected))
            {
                return Err("PGS queue picture differs from its packet and decoder event".into());
            }
            Ok(Some(picture.start_ns))
        }
        _ => Err("PGS queue action disagrees with the decoder event".into()),
    }
}

fn dvd<M>(bridge: &mut DvdReceiver<M>, input: BitmapInput<M>, records: &mut Records) -> Result<()> {
    bridge.provide(input).map_err(|failure| failure.error)?;
    loop {
        match bridge.step(&mut WorkBudget::new(GRANT), &CANCEL)? {
            DvdReceiverStep::Output(output) => {
                if let Some(presentation) = output.presentation()? {
                    let p = presentation.picture;
                    let input = output.packet.input();
                    picture(
                        records,
                        clock(input.clock, input.clock.pts)?,
                        u64::from(p.start_ms),
                        u64::from(p.end_ms),
                        1,
                    );
                    rectangle(
                        records,
                        [p.x.into(), p.y.into(), p.width.into(), p.height.into()],
                        p.forced,
                        p.palette(),
                        (0..p.height).filter_map(|y| p.row(y)),
                    );
                }
                return Ok(());
            }
            DvdReceiverStep::Yield => {}
            _ => return Err("DVD bridge did not finish the provided packet".into()),
        }
    }
}

fn dvb(records: &mut Records, action: DvbQueueAction<'_>) {
    match action {
        DvbQueueAction::Clear { at_ns } => picture(records, (at_ns / 1000) as u64, 0, ABSENT, 0),
        DvbQueueAction::Show(view) => {
            picture(
                records,
                (view.start_ns / 1000) as u64,
                0,
                ABSENT,
                view.rectangles().count(),
            );
            for rect in view.rectangles() {
                let width = usize::from(rect.width);
                rectangle(
                    records,
                    [
                        rect.x.into(),
                        rect.y.into(),
                        rect.width.into(),
                        rect.height.into(),
                    ],
                    false,
                    rect.palette,
                    rect.pixels.chunks(width.max(1)),
                );
            }
        }
    }
}

fn mkv_dvb(
    packet: &MatroskaDvbPacket,
    decoder: &mut ReceiverDecoder,
    records: &mut Records,
) -> Result<()> {
    if records.trace {
        // Matroska carries no decode time.
        println!("# Matroska DVB source DTS None");
    }
    loop {
        match packet.decode(decoder, &mut WorkBudget::new(GRANT), &CANCEL)? {
            MatroskaDvbStep::Yield => {}
            MatroskaDvbStep::Action(action) => {
                dvb(records, action);
                return Ok(());
            }
            MatroskaDvbStep::Decoded => return Ok(()),
            MatroskaDvbStep::Refused(error) => {
                if records.trace {
                    println!("# DVB decoder refused {error:?}");
                }
                return Ok(());
            }
        }
    }
}

fn mkv_dvb_clock(
    track: &MatroskaBitmapTrack,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let mut prefix = Vec::new();
    while let Some(output) = next(player)? {
        let selected = track.dvb_packet(&output, 0)?.is_some();
        prefix.push(output);
        if selected {
            break;
        }
        if prefix.len() >= 65_536 {
            return Err("Matroska DVB clock prefix limit".into());
        }
    }
    let mut probe = MatroskaBitmapClockProbe::new(track, &prefix, prefix.len())?;
    while !probe.poll(&mut WorkBudget::new(GRANT), &CANCEL)? {}
    let mut decoder = track.dvb_decoder()?;
    let mut sequence = 0;
    let mut consume = |output: &Output,
                       packet: Option<MatroskaDvbPacket>,
                       sequence|
     -> Result<u64> {
        let original = track.dvb_packet(output, sequence)?;
        match (original, packet) {
            (None, None) => Ok(sequence),
            (Some(original), Some(clocked)) => {
                if original.packet.payload.as_ref() != clocked.packet.payload.as_ref()
                    || original.packet.generation != clocked.packet.generation
                    || original.packet.sequence != clocked.packet.sequence
                    || original.packet.envelope != clocked.packet.envelope
                    || original.source.delivery != clocked.source.delivery
                    || original
                        .packet
                        .pts
                        .is_some_and(|pts| Some(pts) != clocked.packet.pts)
                {
                    return Err("Matroska DVB probe changed the packet or its known clock".into());
                }
                mkv_dvb(&clocked, &mut decoder, records)?;
                Ok(sequence + 1)
            }
            _ => Err("Matroska DVB probe changed packet selection".into()),
        }
    };
    for (ordinal, output) in prefix.iter().enumerate() {
        sequence = consume(output, probe.dvb_packet(ordinal, sequence)?, sequence)?;
    }
    let mut live = probe.into_live()?;
    drop(prefix);
    while let Some(output) = next(player)? {
        sequence = consume(&output, live.dvb_packet(&output, sequence)?, sequence)?;
    }
    Ok(())
}

#[inline(never)]
fn mp4_dvd(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let mut ids = Vec::new();
    while let Some(output) = next(player)? {
        let Event::Mp4(Mp4Event::Packet(packet)) = output.event() else {
            continue;
        };
        if packet.configuration.track.handler != *b"subp" {
            continue;
        }
        let id = packet.configuration.track.track_id;
        if ids.contains(&id) {
            continue;
        }
        ids.push(id);
        if ids.len() - 1 != case.ordinal {
            continue;
        }
        let mut setup = Mp4DvdSetup::new(&output, ConfigurationLimits::default())?;
        let track = loop {
            if let Some(track) = setup.poll(&mut WorkBudget::new(GRANT))? {
                break track;
            }
        };
        let mut bridge = DvdReceiver::new(pool, track.scope(), track.config())?;
        let mut current = Some(output);
        let mut sequence = 0;
        while let Some(output) = current {
            if let Some(input) = track.packet(&output, sequence)? {
                if sequence == 0 {
                    check_dvd_reset(pool, track.config(), &input)?;
                }
                dvd(&mut bridge, input, records)?;
                sequence += 1;
            }
            current = next(player)?;
        }
        bridge.finish();
        loop {
            match bridge.step(&mut WorkBudget::new(GRANT), &CANCEL)? {
                DvdReceiverStep::Finished => return Ok(()),
                DvdReceiverStep::Yield => {}
                _ => return Err("DVD bridge did not drain".into()),
            }
        }
    }
    Err("MP4 DVD track absent".into())
}

#[inline(never)]
fn matroska(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let accepted = match case.codec {
        Codec::Pgs => "S_HDMV/PGS",
        Codec::Dvb => "S_DVBSUB",
        Codec::Dvd => "S_VOBSUB",
        _ => return Err("Matroska bitmap codec unsupported".into()),
    };
    while let Some(output) = next(player)? {
        let Event::Matroska(triergon_matroska::Event::Configuration(configuration)) =
            output.event()
        else {
            continue;
        };
        let Some(selected) = configuration
            .tracks()
            .iter()
            .filter(|track| track.codec.as_ref() == accepted)
            .nth(case.ordinal)
        else {
            continue;
        };
        let track = MatroskaBitmapTrack::new(&output, selected.number)?;
        if case.kind == "mkv-bitmap-probe" {
            let mut prefix = Vec::new();
            let mut selected_packets = 0;
            let target = Some(TrackKey::Matroska(selected.number));
            let probe_packets = case.option("probe_packets").map(str::parse).transpose()?;
            while probe_packets != Some(selected_packets) {
                let Some(output) = next(player)? else { break };
                if prefix.len() >= 65_536 {
                    return Err("Matroska bitmap probe packet limit".into());
                }
                if output.event().track() == target && output.packet().is_some() {
                    selected_packets += 1;
                }
                prefix.push(output);
            }
            let mut probe = MatroskaBitmapClockProbe::new(&track, &prefix, prefix.len())?;
            while !probe.poll(&mut WorkBudget::new(GRANT), &CANCEL)? {}
            let mut receiver = pgs_receiver(case, pool, track.scope())?;
            let mut sequence = 0;
            for index in 0..prefix.len() {
                if let Some(input) = probe.packet(index, sequence)? {
                    projected_pgs(&mut receiver, input, records)?;
                    sequence += 1;
                }
            }
            let mut live = probe.into_live()?;
            drop(prefix);
            while let Some(output) = next(player)? {
                if let Some(input) = live.packet(&output, sequence)? {
                    projected_pgs(&mut receiver, input, records)?;
                    sequence += 1;
                }
            }
            receiver.finish();
            return drain_pgs(&mut receiver, records);
        }
        match case.codec {
            Codec::Dvb => mkv_dvb_clock(&track, player, records)?,
            Codec::Pgs => {
                let mut receiver = pgs_receiver(case, pool, track.scope())?;
                let mut sequence = 0;
                while let Some(output) = next(player)? {
                    if let Some(input) = track.packet(&output, sequence)? {
                        pgs(&mut receiver, input, records)?;
                        sequence += 1;
                    }
                }
                receiver.finish();
                drain_pgs(&mut receiver, records)?;
            }
            Codec::Dvd => {
                let mut receiver = DvdReceiver::new(
                    pool,
                    track.scope(),
                    DvdBridgeConfig {
                        settings: track.vobsub_settings()?,
                        limits: DvdLimits::default(),
                    },
                )?;
                let mut sequence = 0;
                while let Some(output) = next(player)? {
                    if let Some(input) = track.packet(&output, sequence)? {
                        dvd(&mut receiver, input, records)?;
                        sequence += 1;
                    }
                }
                receiver.finish();
                let _ = receiver.step(&mut WorkBudget::new(GRANT), &CANCEL)?;
            }
            _ => return Err("Matroska bitmap codec unsupported".into()),
        }
        return Ok(());
    }
    Err("Matroska bitmap track absent".into())
}

pub(super) fn ts_map(
    player: &mut Player<ByteSource>,
    ordinal: usize,
    codec: CodecId,
) -> Result<(Output, u16)> {
    while let Some(output) = next(player)? {
        if let Event::Ts(event) = output.event()
            && let TraceEvent::ProgramMap {
                active: true, map, ..
            } = &event.source
            && let Some(stream) = map
                .streams
                .iter()
                .filter(|stream| {
                    if codec == CodecId::Pgs {
                        stream.stream_type == stream_type::HDMV_PGS
                    } else {
                        stream.codec() == Some(codec)
                    }
                })
                .nth(ordinal)
        {
            let pid = stream.pid;
            return Ok((output, pid));
        }
    }
    Err("selected TS stream absent".into())
}

/// Whether `output` is for the TS bridge of `pid`: a PES of the stream, or the fault the
/// player reported for a PES of it that it discarded.
pub(super) fn ts_payload(output: &Output, program: u16, pid: u16) -> bool {
    match output.event() {
        Event::Ts(event) if matches!(event.source, TraceEvent::Pes(_)) => {
            output.event().track() == Some(TrackKey::Ts { program, pid })
        }
        Event::Ts(TsEvent {
            source: TraceEvent::Diagnostic(fault),
            ..
        }) => fault.kind == FaultKind::Pes && fault.pid == Some(pid),
        _ => false,
    }
}

#[inline(never)]
fn ts_dvb(case: &Case<'_>, player: &mut Player<ByteSource>, records: &mut Records) -> Result<()> {
    let (map, pid) = ts_map(player, case.ordinal, CodecId::DvbSubtitle)?;
    let mut receiver = if let Some(service) = case.option("dvb_substream") {
        DvbTsBridge::new_for_service(
            &map,
            pid,
            DvbServiceSelection {
                descriptor: case.option("descriptor").unwrap_or("0").parse()?,
                entry: service.parse()?,
            },
            PACKET_LIMIT,
            &CANCEL,
        )?
    } else {
        DvbTsBridge::new(&map, pid, PACKET_LIMIT, &CANCEL)?
    };
    while let Some(output) = next(player)? {
        if ts_payload(&output, case.program, pid) && receiver.push(&mut Some(output))? {
            loop {
                match receiver.poll(&mut WorkBudget::new(GRANT), &CANCEL)? {
                    DvbStep::Yield => {}
                    DvbStep::Action(action) => dvb(records, action),
                    DvbStep::Refused(error) => {
                        if records.trace {
                            println!("# DVB decoder refused {error:?}");
                        }
                        break;
                    }
                    DvbStep::Discarded(error) => {
                        if records.trace {
                            println!("# DVB PES discarded: {error}");
                        }
                        break;
                    }
                    DvbStep::NeedPacket | DvbStep::Finished => break,
                }
            }
        }
    }
    receiver.finish_input();
    if !matches!(
        receiver.poll(&mut WorkBudget::new(GRANT), &CANCEL)?,
        DvbStep::Finished
    ) {
        return Err("DVB bridge did not drain".into());
    }
    Ok(())
}

#[inline(never)]
fn ts_pgs(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let (map, pid) = ts_map(player, case.ordinal, CodecId::Pgs)?;
    let mut track = PgsTsTrack::new(&map, pid, PACKET_LIMIT)?;
    let mut receiver = pgs_receiver(case, pool, track.scope())?;
    while let Some(output) = next(player)? {
        if ts_payload(&output, case.program, pid) && track.push(&mut Some(output))? {
            loop {
                match track.poll(&mut WorkBudget::new(GRANT))? {
                    PgsTsStep::Yield => {}
                    PgsTsStep::Packet(input) => {
                        if input.sequence == 0 {
                            check_pgs_reset(case, pool, &input)?;
                        }
                        pgs(&mut receiver, input, records)?;
                        break;
                    }
                    PgsTsStep::Discarded(error) => {
                        if records.trace {
                            println!("# PGS PES discarded: {error}");
                        }
                        break;
                    }
                    PgsTsStep::NeedPacket => {
                        return Err("PGS track did not return the provided packet".into());
                    }
                }
            }
        }
    }
    receiver.finish();
    drain_pgs(&mut receiver, records)
}

/// Plays one case. Each path is compiled on its own (`inline(never)`), so a change
/// to one bridge does not move another path's code.
pub(super) fn play(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    match case.kind {
        "mp4-dvd" => mp4_dvd(case, pool, player, records),
        "ts-dvb" => ts_dvb(case, player, records),
        "ts-pgs" => ts_pgs(case, pool, player, records),
        _ => matroska(case, pool, player, records),
    }
}
