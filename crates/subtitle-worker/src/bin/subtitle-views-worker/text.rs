use std::ffi::OsString;
use std::io::{Cursor, Write};
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyPacket, AnyReader, Budget, Bytes, ContainerReader, Error, PacketInfo, SourceId, Time,
    WorkBudget,
};
use triergon_subtitles::context;
use triergon_subtitles::text::{
    Addition, Cue, Encoding, Extractor, Format, Input, Limits, RawClock, Status, Timing,
};

use super::protocol::{Operation, Work};

pub(super) struct Records<'a, 'b> {
    pub work: &'a mut Work,
    pub trace: Option<&'b mut dyn Write>,
}

impl Records<'_, '_> {
    pub fn row(&mut self, kind: u64, words: &[u64], payload: &[u8]) -> Result<(), String> {
        self.work.record(kind, words, payload);
        if let Some(trace) = &mut self.trace {
            let mut write = || -> std::io::Result<()> {
                write!(trace, "{kind}")?;
                for word in words {
                    write!(trace, " {word}")?;
                }
                if !payload.is_empty() {
                    write!(trace, " {}", triergon_harness_protocol::hex(payload))?;
                }
                writeln!(trace)
            };
            write().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn refused(&mut self) -> Result<(), String> {
        self.row(8, &[], &[])
    }
}

pub(super) fn packet_input(payload: Bytes, format: Format) -> Input {
    Input {
        format,
        encoding: match format {
            Format::OpaqueAss => Encoding::Opaque,
            Format::Tx3g => Encoding::Tx3gInBand,
            Format::Utf8Packet | Format::MatroskaWebVtt => Encoding::Utf8,
        },
        payload,
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(1),
        generation: 0,
        sequence: 0,
        configuration_epoch: 0,
        container_position: None,
    }
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    if prefix == "renderer-context-" && arguments.is_empty() {
        return Ok(Operation {
            name: "renderer-context",
            run: Box::new(move |work, trace| {
                let mut records = Records { work, trace };
                let Ok(context) = renderer_context(input) else {
                    return records.refused();
                };
                let header = context
                    .header()
                    .codec_private
                    .as_deref()
                    .unwrap_or_default();
                records.row(2, &[0, header.len() as u64], header)?;
                for font in context
                    .attachments()
                    .iter()
                    .filter(|file| is_font(&file.media_type))
                {
                    records.row(2, &[1, font.name.len() as u64], &font.name)?;
                    records.row(2, &[2, font.data.len() as u64], &font.data)?;
                }
                records.row(7, &[], &[])
            }),
        });
    }
    let [mode, format, rest @ ..] = arguments else {
        return Err(format!("usage: {prefix} raw|mkv|mov FORMAT [SOURCE FACTS]"));
    };
    let format = match format.to_str() {
        Some("ass") => Format::OpaqueAss,
        Some("utf8") => Format::Utf8Packet,
        Some("vtt") => Format::MatroskaWebVtt,
        Some("tx3g") => Format::Tx3g,
        _ => return Err("invalid extraction format".into()),
    };
    let mode = mode.to_str().ok_or("invalid extraction mode")?;
    let pool = Budget::new(128 * 1024 * 1024).map_err(|error| error.to_string())?;
    let run: super::protocol::Action<'a> = match mode {
        "raw" => {
            let [start, duration, clock, limit, addition] = rest else {
                return Err("expected raw timing, clock, limit and addition".into());
            };
            let mut sample = packet_input(
                Bytes::copy_from_slice(&pool, input).map_err(|error| error.to_string())?,
                format,
            );
            sample.timing = match clock.to_str().ok_or("bad source clock")? {
                "-" => Timing::exact(time(start)?, time(duration)?),
                value => {
                    let fields: Vec<_> = value.split(':').collect();
                    let [num, den, pts, dts, dur] = fields.as_slice() else {
                        return Err("bad source clock".into());
                    };
                    Timing::from_ticks(RawClock {
                        numerator: num.parse::<u64>().map_err(|error| error.to_string())?,
                        denominator: den.parse::<u64>().map_err(|error| error.to_string())?,
                        pts_ticks: optional_tick(pts)?,
                        dts_ticks: optional_tick(dts)?,
                        duration_ticks: optional_tick(dur)?,
                    })
                    .map_err(|error| error.to_string())?
                }
            };
            let limits = Limits {
                max_input_bytes: limit
                    .to_str()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(Limits::default().max_input_bytes),
                ..Limits::default()
            };
            if let Some((id, data)) = addition.to_str().and_then(|s| s.split_once(':')) {
                let id = id.parse::<u64>().map_err(|error| error.to_string())?;
                let bytes = decode_hex(data)?;
                sample.addition = Some(Addition {
                    id,
                    bytes: Bytes::copy_from_slice(&pool, &bytes)
                        .map_err(|error| error.to_string())?,
                });
            }
            Box::new(move |work, trace| {
                let mut records = Records { work, trace };
                match extract(&pool, sample.clone(), limits) {
                    Ok(cue) => source_fields(&mut records, &cue),
                    Err(_) => records.refused(),
                }
            })
        }
        "mkv" | "mov" if rest.is_empty() => Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            demux_extract(&mut records, input, format, &pool)
        }),
        _ => return Err("invalid extraction input".into()),
    };
    Ok(Operation {
        name: "text-extraction",
        run,
    })
}

fn optional_tick(value: &str) -> Result<Option<i128>, String> {
    if value == "-" {
        return Ok(None);
    }
    value
        .parse::<i128>()
        .map(Some)
        .map_err(|error| error.to_string())
}

fn time(value: &OsString) -> Result<Option<Time>, String> {
    let value = value.to_str().ok_or("bad exact time")?;
    if value == "-" {
        return Ok(None);
    }
    let (num, den) = value.split_once('/').ok_or("bad exact time")?;
    let num = num.parse::<i128>().map_err(|error| error.to_string())?;
    let den = den.parse::<u128>().map_err(|error| error.to_string())?;
    Time::new(num, den)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn decode_hex(data: &str) -> Result<Vec<u8>, String> {
    data.as_bytes()
        .as_chunks::<2>()
        .1
        .is_empty()
        .then_some(())
        .ok_or("odd hex length")?;
    data.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
            u8::from_str_radix(pair, 16).map_err(|error| error.to_string())
        })
        .collect()
}

pub(super) fn extract(pool: &Budget, input: Input, limits: Limits) -> triergon::Result<Cue> {
    let cancel = AtomicBool::new(false);
    let mut extractor = Extractor::new(pool, limits, input.generation);
    extractor.provide(input).map_err(|refused| refused.error)?;
    loop {
        match extractor.step(&mut WorkBudget::new(4096), &cancel)? {
            Status::Cue(cue) => return Ok(cue),
            Status::Yield => {}
            _ => return Err(Error::Invalid("extractor did not publish a cue")),
        }
    }
}

fn optional_i128(words: &mut Vec<u64>, value: Option<i128>) {
    words.push(value.is_some().into());
    if let Some(value) = value {
        words.extend([value as u64, (value >> 64) as u64]);
    }
}

fn source_fields(records: &mut Records<'_, '_>, cue: &Cue) -> Result<(), String> {
    let mut words = vec![
        cue.raw_range.start as u64,
        cue.raw_range.end as u64,
        cue.text_range.start as u64,
        cue.text_range.end as u64,
        match cue.effective_encoding {
            Encoding::Utf8 => 0,
            Encoding::Utf16Be => 1,
            Encoding::Opaque => 2,
            _ => return Err("unexpected extraction encoding".into()),
        },
    ];
    for time in [cue.start, cue.end, cue.duration] {
        optional_i128(&mut words, time.map(Time::numerator));
        if let Some(time) = time {
            let den = time.denominator();
            words.extend([den as u64, (den >> 64) as u64]);
        }
    }
    if let Some(clock) = cue.input.timing.raw_clock() {
        words.extend([1, clock.numerator, clock.denominator]);
        for value in [clock.pts_ticks, clock.dts_ticks, clock.duration_ticks] {
            optional_i128(&mut words, value);
        }
    } else {
        words.push(0);
    }
    let text = cue.decoded_utf8.as_deref().unwrap_or_default();
    words.push(text.len() as u64);
    records.row(1, &words, text)?;
    for (tag, field) in [&cue.identifier, &cue.settings, &cue.comments]
        .into_iter()
        .enumerate()
    {
        if let Some(field) = field {
            let data = field.view(&cue.input).map_err(|error| error.to_string())?;
            records.row(
                3,
                &[tag as u64, field.range.start as u64, field.range.end as u64],
                &data,
            )?;
        }
    }
    for modifier in cue.modifiers.iter() {
        records.row(
            3,
            &[
                3,
                u32::from_be_bytes(modifier.kind).into(),
                modifier.range.start as u64,
                modifier.range.end as u64,
            ],
            &[],
        )?;
    }
    Ok(())
}

fn demux_extract(
    records: &mut Records<'_, '_>,
    input: &[u8],
    format: Format,
    pool: &Budget,
) -> Result<(), String> {
    let Ok(mut reader) = triergon::open_blocking(Cursor::new(input), pool) else {
        return records.refused();
    };
    let mut selected = None;
    let mut ordinal = 0;
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(_) => return records.refused(),
        };
        let track = if let Some(id) = selected {
            id
        } else {
            let desired = match format {
                Format::OpaqueAss => [triergon::CodecId::Ass, triergon::CodecId::Ssa],
                Format::Utf8Packet => [triergon::CodecId::Utf8Text; 2],
                Format::MatroskaWebVtt => [triergon::CodecId::WebVtt; 2],
                Format::Tx3g => [triergon::CodecId::Tx3g; 2],
            };
            let Some(track) = (0..reader.reader().track_count())
                .filter_map(|i| reader.reader().track(i))
                .find(|track| desired.contains(&track.codec()))
            else {
                return records.refused();
            };
            *selected.insert(track.id())
        };
        if packet.track() != track {
            continue;
        }
        let payload =
            Bytes::copy_from_slice(pool, packet.payload()).map_err(|error| error.to_string())?;
        let mut source = packet_input(payload, format);
        let mut base = packet.time_base();
        let mut pts = packet.pts();
        let mut duration = packet.duration();
        if let (AnyReader::Matroska(mkv), AnyPacket::Matroska(packet)) = (reader.reader(), &packet)
        {
            let scale = mkv.timestamp_scale_ns();
            let divisor = i64::try_from(scale).map_err(|error| error.to_string())?;
            base =
                triergon::TimeBase::new(scale, 1_000_000_000).map_err(|error| error.to_string())?;
            pts = pts.map(|ns| ns.div_euclid(divisor));
            duration = duration.map(|ns| ns.div_euclid(divisor));
            if let Some(group) = &packet.group
                && let Some(addition) = group.additions().find(|addition| addition.id() == 1)
            {
                source.addition = Some(Addition {
                    id: addition.id(),
                    bytes: Bytes::copy_from_slice(pool, addition.data())
                        .map_err(|error| error.to_string())?,
                });
            }
        }
        source.timing = Timing::from_ticks(RawClock {
            numerator: base.numerator(),
            denominator: base.denominator(),
            pts_ticks: pts.map(i128::from),
            dts_ticks: None,
            duration_ticks: duration.map(i128::from),
        })
        .map_err(|error| error.to_string())?;
        let Ok(cue) = extract(pool, source, Limits::default()) else {
            records.row(8, &[ordinal], &[])?;
            ordinal += 1;
            continue;
        };
        let clock = cue.input.timing.raw_clock().ok_or("missing cue clock")?;
        let text = cue
            .decoded_utf8
            .as_deref()
            .unwrap_or(cue.input.payload.as_ref());
        let comments = cue
            .comments
            .as_ref()
            .map_or(0, |comments| comments.range.len());
        records.row(
            1,
            &[
                ordinal,
                clock.numerator,
                clock.denominator,
                clock.pts_ticks.is_some().into(),
                clock.pts_ticks.unwrap_or(0) as u64,
                clock.duration_ticks.unwrap_or(0) as u64,
                text.len() as u64,
                comments as u64,
            ],
            text,
        )?;
        for (tag, field) in [&cue.identifier, &cue.settings].into_iter().enumerate() {
            if let Some(field) = field {
                let data = field.view(&cue.input).map_err(|error| error.to_string())?;
                records.row(3, &[tag as u64, data.len() as u64], &data)?;
            }
        }
        ordinal += 1;
    }
    records.row(7, &[], &[])
}

fn is_font(mime: &[u8]) -> bool {
    matches!(
        mime,
        b"application/x-truetype-font"
            | b"application/x-font-ttf"
            | b"application/x-font-otf"
            | b"application/x-font-opentype"
            | b"application/vnd.ms-opentype"
            | b"font/ttf"
            | b"font/otf"
            | b"font/collection"
    )
}

fn renderer_context(input: &[u8]) -> triergon::Result<context::Context> {
    let pool = Budget::new(128 * 1024 * 1024)?;
    let mut reader = triergon::open_blocking(Cursor::new(input), &pool)?;
    let _ = reader.next_packet()?;
    let AnyReader::Matroska(reader) = reader.reader() else {
        return Err(Error::Invalid("renderer context requires Matroska"));
    };
    let track = reader
        .tracks()
        .iter()
        .find(|track| {
            track.kind == 17
                && matches!(
                    &*track.codec,
                    "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA"
                )
        })
        .ok_or(Error::Invalid("no ASS track"))?;
    let copy = |data: &[u8]| Bytes::copy_from_slice(&pool, data);
    let mut builder = context::Builder::new(&pool, context::Limits::default());
    builder.provide_header(&mut Some(context::Header {
        identity: context::Identity {
            source: reader.source(),
            generation: reader.generation(),
            configuration_epoch: 0,
            track_number: track.number,
            track_uid: track.uid,
        },
        codec_id: copy(track.codec.as_bytes())?,
        codec_private: track
            .has_codec_private()
            .then(|| copy(&track.codec_private))
            .transpose()?,
        name: (!track.name.is_empty())
            .then(|| copy(track.name.as_bytes()))
            .transpose()?,
        language: Some(copy(track.language.as_bytes())?),
        language_bcp47: track
            .language_bcp47
            .as_ref()
            .map(|s| copy(s.as_bytes()))
            .transpose()?,
        charset_hint: None,
        position: None,
    }))?;
    let cancel = AtomicBool::new(false);
    for item in reader.attachments() {
        builder.provide_attachment(&mut Some(context::Attachment {
            uid: item.uid()?,
            name: copy(item.filename()?.as_bytes())?,
            media_type: copy(item.media_type()?.as_bytes())?,
            description: item
                .description()?
                .map(|s| copy(s.as_bytes()))
                .transpose()?,
            data: copy(item.data()?)?,
            position: None,
        }))?;
        builder.poll(&mut WorkBudget::new(4), &cancel)?;
    }
    for tag in reader
        .catalogue()
        .iter()
        .flat_map(|document| document.tags())
    {
        if let Some(targets) = tag.targets() {
            let tracks = targets.uids(0x63c5).collect::<triergon::Result<Vec<_>>>()?;
            if !tracks.is_empty() && !tracks.contains(&track.uid) {
                continue;
            }
        }
        for item in tag.values() {
            if let Some(value) = item.text()? {
                builder.provide_metadata(&mut Some(context::Metadata {
                    key: copy(item.name()?.as_bytes())?,
                    value: copy(value.as_bytes())?,
                    position: None,
                }))?;
                builder.poll(&mut WorkBudget::new(4), &cancel)?;
            }
        }
    }
    builder.finish()?;
    builder.poll(&mut WorkBudget::new(4), &cancel)?;
    builder
        .take_output()
        .ok_or(Error::Invalid("no renderer context"))
}
