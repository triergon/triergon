use std::{ffi::OsString, io::Write, sync::atomic::AtomicBool};

use triergon::framing::BuildStep;
use triergon::framing::ac3 as framing;
use triergon::{
    Budget, Bytes, ContainerReader, InputWindow, MediaKind, PacketInfo, SourceCapabilities,
    SourceId, Step, WorkBudget, open_sequential,
};
use triergon_codec::layout::{native_order, vorbis_order};
use triergon_codec::{AudioFrame, DownmixLevels, SurroundEncoding};
use triergon_codec_ac3::{self as ac3, DownmixInfo, DownmixType, FrameInfo, Syntax};
use triergon_session::TrackDecoder;

use triergon_codec_aac::adts;
use triergon_harness_protocol::digest::fold_bytes;

use super::protocol::{Operation, Work};

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    match prefix {
        "adts-sync-" if arguments.is_empty() => Ok(adts_sync(input)),
        "ac3-features-" if arguments.is_empty() => Ok(ac3_tools(input)),
        "ac3-header-" if arguments.is_empty() => Ok(ac3_header(input)),
        "ac3-frame-info-" if matches!(arguments, [lane] if lane == "ac3" || lane == "eac3") => {
            Ok(audio_operation(input, Lane::Ac3Info))
        }
        "layouts-" => {
            let [lane] = arguments else {
                return Err("layouts requires a codec lane".into());
            };
            let lane = match lane.to_str() {
                Some("aac") => Lane::Aac,
                Some("ac3" | "eac3") => Lane::Ac3,
                Some("dts") => Lane::Dts,
                Some("flac" | "mp1" | "mp2" | "mp3" | "opus" | "vorbis") => Lane::File,
                _ => return Err("unknown audio layout lane".into()),
            };
            Ok(audio_operation(input, lane))
        }
        _ => Err(format!("usage: {prefix} operation is unavailable")),
    }
}

fn adts_sync(input: &[u8]) -> Operation<'_> {
    Operation {
        name: "adts-sync",
        run: Box::new(move |work, mut trace| {
            let mut from = 0;
            while let Some((offset, header)) = adts::find_frame(input, from) {
                from = offset + header.frame_length;
                let packet = &input[offset..from.min(input.len())];
                let fields = [
                    offset as u64,
                    header.frame_length as u64,
                    u64::from(header.profile),
                    u64::from(header.sf_index),
                    u64::from(header.channel_config),
                    packet.len() as u64,
                ];
                work.record(1, &fields, packet);
                if let Some(trace) = &mut trace {
                    let mut digest = 0;
                    fold_bytes(&mut digest, packet);
                    let [offset, length, profile, rate, channels, size] = fields;
                    writeln!(
                        trace,
                        "1 {offset} {length} {profile} {rate} {channels} {size} {digest}"
                    )
                    .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        }),
    }
}

pub(super) fn command(command: &str, _arguments: &[OsString]) -> Result<(), String> {
    Err(format!("usage: {command} operation is unavailable"))
}

fn record(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    fields: &[u64],
    payload: &[u8],
) -> Result<(), String> {
    work.record(kind, fields, payload);
    if let Some(trace) = trace {
        write!(trace, "{kind}").map_err(|error| error.to_string())?;
        for value in fields {
            write!(trace, " {value}").map_err(|error| error.to_string())?;
        }
        if !payload.is_empty() {
            write!(trace, " {}", triergon_harness_protocol::hex(payload))
                .map_err(|error| error.to_string())?;
        }
        writeln!(trace).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn ac3_header(input: &[u8]) -> Operation<'_> {
    Operation {
        name: "ac3-frame-header",
        run: Box::new(move |work, mut trace| {
            let mut pos = 0;
            while pos < input.len() {
                let rest = &input[pos..];
                if !rest.starts_with(&[0x0b, 0x77]) {
                    pos += 1;
                    continue;
                }
                let Ok(header) = ac3::parse_header(rest) else {
                    record(work, &mut trace, 8, &[pos as u64], &[])?;
                    break;
                };
                let extent = framing::probe_header(rest)
                    .map_or(header.frame_bytes, |observed| observed.frame_bytes);
                let fields = [
                    pos as u64,
                    u64::from(header.syntax == Syntax::Eac3),
                    u64::from(header.bsid),
                    header.frame_bytes as u64,
                    u64::from(header.sample_rate),
                    header.num_blocks as u64,
                    u64::from(header.acmod),
                    u64::from(header.lfeon),
                    extent.min(rest.len()) as u64,
                ];
                record(work, &mut trace, 3, &fields, &[])?;
                pos += extent;
            }
            Ok(())
        }),
    }
}

#[derive(Clone, Copy)]
enum Lane {
    Ac3Info,
    Ac3,
    Aac,
    Dts,
    File,
}

fn audio_operation(input: &[u8], lane: Lane) -> Operation<'_> {
    let info = matches!(lane, Lane::Ac3Info);
    Operation {
        name: if info {
            "ac3-frame-info"
        } else {
            "codec-layouts"
        },
        run: Box::new(move |work, mut trace| {
            let mut frame = AudioFrame::default();
            let mut refused = false;
            match lane {
                Lane::Ac3 | Lane::Ac3Info => {
                    let mut decoder = ac3::Decoder::new();
                    audio_frames(
                        input,
                        |rest| ac3::access_unit_len(rest).ok(),
                        |packet| {
                            frame.clear();
                            match decoder.decode(packet, &mut frame) {
                                Ok(decoded) => {
                                    if info {
                                        frame_info(work, &mut trace, &decoded, packet)?;
                                    }
                                    layout(
                                        work,
                                        &mut trace,
                                        &frame,
                                        decoded.downmix.as_ref().map(DownmixInfo::levels),
                                    )?;
                                    if let Some(mix) = decoded.downmix {
                                        downmix(work, &mut trace, &mix)?;
                                    }
                                }
                                Err(_) => refused = true,
                            }
                            Ok(())
                        },
                    )?;
                }
                Lane::Aac => {
                    let mut decoder = triergon_codec_aac::Decoder::new();
                    audio_frames(
                        input,
                        |rest| {
                            adts::AdtsHeader::parse(rest)
                                .ok()
                                .map(|header| header.frame_length)
                        },
                        |packet| {
                            frame.clear();
                            if decoder.decode(packet, &mut frame).is_ok() {
                                layout(work, &mut trace, &frame, None)
                            } else {
                                refused = true;
                                Ok(())
                            }
                        },
                    )?;
                }
                Lane::Dts => {
                    let mut decoder = triergon_codec_dca::Decoder::new();
                    audio_frames(input, dts_extent, |packet| {
                        frame.clear();
                        if decoder.decode(packet, &mut frame).is_ok() {
                            layout(work, &mut trace, &frame, None)
                        } else {
                            refused = true;
                            Ok(())
                        }
                    })?;
                }
                Lane::File => refused = file_layouts(input, &mut frame, work, &mut trace)?,
            }
            if refused && work.packets == 0 {
                record(work, &mut trace, 8, &[], &[])?;
            }
            Ok(())
        }),
    }
}

fn dts_extent(input: &[u8]) -> Option<usize> {
    let prefix = u64::from_be_bytes(input.get(..8)?.try_into().ok()?);
    (prefix >> 32 == 0x7ffe_8001).then_some(((prefix >> 4) as usize & 0x3fff) + 1)
}

fn audio_frames(
    input: &[u8],
    extent: impl Fn(&[u8]) -> Option<usize>,
    mut receive: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut rest = input;
    while !rest.is_empty() {
        if let Some(length) = extent(rest) {
            let (packet, tail) = rest.split_at(length.min(rest.len()));
            receive(packet)?;
            rest = tail;
        } else {
            rest = &rest[1..];
        }
    }
    Ok(())
}

fn file_layouts(
    input: &[u8],
    frame: &mut AudioFrame,
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
) -> Result<bool, String> {
    let pool = Budget::new(256 << 20).map_err(|error| error.to_string())?;
    let Ok(mut input) = open_sequential(input, &pool) else {
        return Ok(true);
    };
    let mut decoder = None;
    let mut refused = false;
    loop {
        let packet = match input.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(_) => {
                refused = true;
                break;
            }
        };
        if decoder.is_none() {
            let reader = input.reader();
            let track = (0..reader.track_count())
                .filter_map(|index| reader.track(index))
                .find(|track| track.kind() == MediaKind::Audio);
            let Some(track) = track else {
                return Ok(true);
            };
            let Ok(codec) = TrackDecoder::new(track) else {
                return Ok(true);
            };
            decoder = Some((track.id(), codec));
        }
        if let Some((id, codec)) = &mut decoder
            && packet.track() == *id
        {
            frame.clear();
            match codec.decode(&packet, frame) {
                Ok(levels) => layout(work, trace, frame, levels)?,
                Err(_) => refused = true,
            }
        }
    }
    if let Some((_, decoder)) = &mut decoder {
        frame.clear();
        match decoder.flush(frame) {
            Ok(()) => layout(work, trace, frame, None)?,
            Err(_) => refused = true,
        }
    }
    Ok(refused)
}

fn frame_info(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    info: &FrameInfo,
    packet: &[u8],
) -> Result<(), String> {
    let (mode, matrix) = match framing::probe_header(packet) {
        Ok(header) => match header.details {
            framing::Ac3FrameDetails::Ac3 {
                bsmod,
                surround_mode,
                ..
            } => (bsmod, u64::from(surround_mode == Some(2))),
            framing::Ac3FrameDetails::Eac3 { .. } => packet
                .get(..header.frame_bytes)
                .and_then(eac3_modes)
                .unwrap_or_default(),
        },
        Err(_) => (0, 0),
    };
    let service = if mode == 7 && info.acmod > 1 { 8 } else { mode };
    record(
        work,
        trace,
        4,
        &[
            u64::from(info.syntax == Syntax::Eac3),
            u64::from(info.bsid),
            u64::from(info.acmod),
            u64::from(info.lfeon),
            u64::from(info.dialnorm),
            u64::from(info.sample_rate),
            info.samples as u64,
            info.substreams as u64,
            u64::from(info.crc_ok),
            u64::from(info.concealed.is_some()),
            u64::from(service),
            matrix,
        ],
        &[],
    )
}

fn eac3_modes(input: &[u8]) -> Option<(u8, u64)> {
    // The E-AC-3 BSI exposes service and matrix signaling separately from
    // the decoded downmix coefficients.
    let pool = Budget::new(64 << 10).ok()?;
    let source = SourceId(0);
    let end = SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: false,
    };
    let mut reader = framing::Ac3FrameReader::new(
        &pool,
        source,
        end,
        framing::Ac3StreamSelection::Eac3Only,
        framing::Ac3FrameLimits {
            maximum_frame_bytes: 4096,
            maximum_frames: 1,
            maximum_input_window: 4096,
        },
    )
    .ok()?;
    let token = reader.request().token;
    let data = Bytes::copy_from_slice(&pool, input).ok()?;
    reader
        .provide_owned(InputWindow::new(source, token, 0, data).ok()?)
        .ok()?;
    let cancel = AtomicBool::new(false);
    let mut work = WorkBudget::new(32 << 10);
    let Step::Ready(frame) = reader.step(&mut work, &cancel).ok()? else {
        return None;
    };
    let limits = framing::Eac3BsiLimits {
        maximum_bsi_bits: 4096 * 8,
        maximum_frame_bytes: 4096,
    };
    let mut parser = framing::Eac3BsiParser::new(&pool, frame, limits).ok()?;
    let BuildStep::Ready(bsi) = parser.step(&mut work, &cancel).ok()? else {
        return None;
    };
    let facts = bsi.facts();
    let matrix = match (
        facts.surround_mode,
        facts.headphone_mode,
        facts.surround_ex_mode,
    ) {
        (_, Some(2), _) => 6,
        (Some(2), _, _) => 1,
        (_, _, Some(2)) => 5,
        (_, _, Some(3)) => 4,
        _ => 0,
    };
    Some((facts.bitstream_mode.unwrap_or(0), matrix))
}

fn gain(level: f64) -> u64 {
    (level * 100_000.0).round() as u64
}

fn encoding(encoding: SurroundEncoding) -> u64 {
    match encoding {
        SurroundEncoding::None => 0,
        SurroundEncoding::Dolby => 1,
        SurroundEncoding::ProLogicII => 2,
    }
}

fn layout(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    frame: &AudioFrame,
    levels: Option<DownmixLevels>,
) -> Result<(), String> {
    if frame.samples_per_channel == 0 {
        return Ok(());
    }
    let layout = &frame.layout;
    let mask = layout.mask();
    let order = if mask == 0 {
        0
    } else if layout.speakers().windows(2).all(|pair| pair[0] < pair[1]) {
        1
    } else {
        2
    };
    let name = layout.name();
    let mut fields = vec![
        u64::from(frame.sample_rate),
        layout.channels() as u64,
        order,
        mask,
    ];
    for speaker in layout.speakers() {
        fields.push(if order == 0 {
            u64::MAX
        } else {
            *speaker as u64
        });
    }
    fields.push(name.len() as u64);
    record(work, trace, 4, &fields, name.as_bytes())?;

    for coded in [Some(layout.speakers()), vorbis_order(layout.channels())] {
        let mut fields = Vec::new();
        if let Some((ordered, permutations)) = coded.and_then(native_order) {
            fields.push(ordered.channels() as u64);
            fields.extend(
                permutations[..ordered.channels()]
                    .iter()
                    .map(|&plane| u64::from(plane)),
            );
        } else {
            fields.push(0);
        }
        record(work, trace, 3, &fields, &[])?;
    }
    if let Some(levels) = levels {
        record(
            work,
            trace,
            3,
            &[
                gain(f64::from(levels.center)),
                gain(f64::from(levels.surround)),
                gain(f64::from(levels.lfe)),
                encoding(levels.encoding),
            ],
            &[],
        )?;
    }
    Ok(())
}

fn downmix(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    info: &DownmixInfo,
) -> Result<(), String> {
    let mode = match info.preferred_downmix_type {
        Some(DownmixType::LoRo) => 1,
        Some(DownmixType::LtRt) => 2,
        Some(DownmixType::DplII) => 3,
        _ => 0,
    };
    let fields = [
        mode,
        info.center_mix_level.map_or(u64::MAX, gain),
        info.center_mix_level_ltrt.map_or(u64::MAX, gain),
        info.surround_mix_level.map_or(u64::MAX, gain),
        info.surround_mix_level_ltrt.map_or(u64::MAX, gain),
        info.lfe_mix_level.map_or(u64::MAX, gain),
    ];
    record(work, trace, 3, &fields, &[])
}

fn ac3_tools(input: &[u8]) -> Operation<'_> {
    Operation {
        name: "ac3-features",
        run: Box::new(move |work, mut trace| {
            let mut decoder = ac3::Decoder::new();
            let mut frame = AudioFrame::default();
            let mut offset = 0;
            while offset < input.len() {
                let bytes = &input[offset..];
                let Ok(header) = ac3::parse_header(bytes) else {
                    return record(work, &mut trace, 8, &[offset as u64], &[]);
                };
                let packet = &bytes[..header.frame_bytes.min(bytes.len())];
                frame.clear();
                let Ok(info) = decoder.decode(packet, &mut frame) else {
                    return record(work, &mut trace, 8, &[offset as u64], &[]);
                };
                let f = info.features;
                let flags = [
                    f.coupling,
                    f.phase_flags,
                    f.rematrix,
                    f.block_switch,
                    f.dither,
                    f.delta_ba,
                    f.dynrng,
                    f.spx,
                    f.spx_atten,
                    f.aht,
                    f.gaq,
                    f.tpnp,
                    f.frame_expstr,
                    f.skip_fields,
                    f.ecpl,
                    f.overrun,
                ];
                let mut fields = [0; 17];
                fields[0] = offset as u64;
                for (out, flag) in fields[1..].iter_mut().zip(flags) {
                    *out = u64::from(flag);
                }
                record(work, &mut trace, 4, &fields, &[])?;
                offset += packet.len();
            }
            Ok(())
        }),
    }
}
