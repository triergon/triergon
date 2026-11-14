//! Write one AVC or HEVC track as Annex B for byte-level comparisons.
use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, Result, SourceId, SourceLocation, Step, WorkBudget,
    blocking::{Blocking, Feed, seekable_capabilities},
    container::{CodecId, Event, PacketInfo, TrackInfo},
};
use triergon_framing::{
    AvcConfigLimits, AvcConfigSource, BuildStep, Codec, HvcConfigLimits, HvcConfigSource,
    NalPacket, NalPacketConfiguration, NalPacketLimits, NalPacketSession,
    compat::nal::{ReceiverAvcConfigParser, ReceiverHevcConfigParser},
};
use triergon_isobmff::{
    BoxStep, EntryLayout, Limits, Reader, ReaderEvent, ReaderLimits, SampleDescription,
    SampleReader, SampleStep,
    compat::mov::recovery::{RecoverySampleExtent, RecoverySampleRead, RunRecoveryProfile},
    open::{MovieOpen, OpenLimits, OpenStep},
};

static CANCEL: AtomicBool = AtomicBool::new(false);
const SOURCE: SourceId = SourceId(1);
const WORK: u64 = 4096;
const CONFIG_BYTES: usize = 1 << 20;
const PACKET_BYTES: usize = 16 << 20;
const OUTPUT_BYTES: usize = 16 << 20;
const PACKET_LIMITS: NalPacketLimits = NalPacketLimits {
    maximum_packet_bytes: PACKET_BYTES,
    maximum_output_bytes: PACKET_BYTES + CONFIG_BYTES,
    maximum_nals: 65_536,
    maximum_segments: 131_072,
};

fn finish<T>(mut step: impl FnMut(&mut WorkBudget) -> Result<BuildStep<T>>) -> Result<T> {
    loop {
        match step(&mut WorkBudget::new(WORK))? {
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Yield => {}
            BuildStep::Complete => return Err(Error::Invalid("framing output missing")),
        }
    }
}

fn avc_configuration(
    pool: &Budget,
    raw: Bytes,
    offset: u64,
    update: bool,
) -> Result<triergon_framing::compat::nal::ReceiverAvcConfiguration> {
    let source = AvcConfigSource {
        source: SOURCE,
        generation: 0,
        offset,
    };
    let limits = AvcConfigLimits {
        maximum_record_bytes: CONFIG_BYTES,
        maximum_nals: 541,
        maximum_nal_bytes: u16::MAX.into(),
    };
    let mut parser = if update {
        ReceiverAvcConfigParser::new_update(pool, source, raw, limits)?
    } else {
        ReceiverAvcConfigParser::new(pool, source, raw, limits)?
    };
    finish(|work| parser.step(work, &CANCEL))
}

fn configuration(
    pool: &Budget,
    codec: Codec,
    raw: Bytes,
    offset: u64,
) -> Result<NalPacketConfiguration> {
    match codec {
        Codec::Avc => {
            avc_configuration(pool, raw, offset, false).map(NalPacketConfiguration::ReceiverAvc)
        }
        Codec::Hevc => {
            let source = HvcConfigSource {
                source: SOURCE,
                generation: 0,
                offset,
            };
            let limits = HvcConfigLimits {
                maximum_record_bytes: CONFIG_BYTES,
                maximum_arrays: u8::MAX.into(),
                maximum_nals: 65_536,
                maximum_nal_bytes: u16::MAX.into(),
            };
            let mut parser = ReceiverHevcConfigParser::new(pool, source, raw, limits)?;
            finish(|work| parser.step(work, &CANCEL)).map(NalPacketConfiguration::ReceiverHevc)
        }
    }
}

fn sample_configuration(
    descriptions: &[SampleDescription],
    codec: Codec,
    index: u32,
) -> Result<(Bytes, u64)> {
    let entry = descriptions
        .iter()
        .find(|entry| entry.index == index)
        .ok_or(Error::Invalid("sample description is missing"))?;
    let (formats, kind) = match codec {
        Codec::Avc => ([*b"avc1", *b"avc3"], *b"avcC"),
        Codec::Hevc => ([*b"hvc1", *b"hev1"], *b"hvcC"),
    };
    if !formats.contains(&entry.format) {
        return Err(Error::Unsupported("sample description changed video codec"));
    }
    let mut children = entry.extensions(EntryLayout::Visual, Limits::default())?;
    loop {
        let mut work = WorkBudget::new(WORK);
        match children.poll(&mut work)? {
            BoxStep::Box(child) if child.header().kind == kind => {
                let body = child.payload();
                if body.len() > CONFIG_BYTES {
                    return Err(Error::Limit {
                        requested: body.len(),
                        maximum: CONFIG_BYTES,
                    });
                }
                return Ok((
                    child
                        .bytes()
                        .slice(usize::from(child.header().header_size)..child.bytes().len())?,
                    child.header().payload_range()?.start,
                ));
            }
            BoxStep::Box(_) | BoxStep::Yield => {}
            BoxStep::End => return Err(Error::Invalid("video configuration is missing")),
        }
    }
}

fn write_annex_b(path: &Path, output: &mut impl Write) -> Result<()> {
    let mut file = File::open(path)?;
    let pool = Budget::new(64 << 20)?;
    let capabilities = seekable_capabilities(&mut file)?;
    let reader = Reader::new(&pool, SOURCE, capabilities, ReaderLimits::default(), 65_536)?;
    let mut input = Blocking::seekable(reader, file, &pool);
    while let Some(event) = input.next_event()? {
        if matches!(event, Event::Other(ReaderEvent::Movie)) {
            break;
        }
    }
    let (track_id, codec) = input
        .reader()
        .tracks()
        .iter()
        .find_map(|track| {
            let codec = match track.codec() {
                CodecId::H264 => Codec::Avc,
                CodecId::Hevc => Codec::Hevc,
                _ => return None,
            };
            Some((track.id(), codec))
        })
        .ok_or(Error::Unsupported("no AVC or HEVC video track"))?;
    let mut session = None;
    let mut current_description = None;
    let mut written = 0usize;
    while let Some(packet) = input.next_packet()? {
        if packet.track() != track_id {
            continue;
        }
        let sample = packet.sample();
        let mut new_extradata = None;
        let mut update = None;
        if current_description != Some(sample.description_index) {
            let track = input
                .reader()
                .tracks()
                .iter()
                .find(|track| track.id() == track_id)
                .ok_or(Error::Invalid("video track disappeared"))?;
            let (raw, offset) =
                sample_configuration(track.descriptions(), codec, sample.description_index)?;
            if session.is_none() {
                session = Some(NalPacketSession::new(
                    &pool,
                    configuration(&pool, codec, raw, offset)?,
                    PACKET_LIMITS,
                )?);
            } else if codec == Codec::Avc {
                update = Some(avc_configuration(&pool, raw.clone(), offset, true)?);
                new_extradata = Some(raw);
            } else {
                return Err(Error::Unsupported("HEVC sample description change"));
            }
            current_description = Some(sample.description_index);
        }
        let source = NalPacket {
            bytes: packet.bytes().clone(),
            location: SourceLocation {
                source: SOURCE,
                offset: sample.range.start,
            },
            generation: 0,
            fields: (),
            new_extradata,
        };
        let session = session
            .as_mut()
            .ok_or(Error::Invalid("video session is missing"))?;
        let mut job = session.receiver_packet(source, update)?;
        let framed = finish(|work| job.step(work, &CANCEL))?;
        written = written
            .checked_add(framed.bytes().len())
            .ok_or(Error::Overflow)?;
        if written > OUTPUT_BYTES {
            return Err(Error::Limit {
                requested: written,
                maximum: OUTPUT_BYTES,
            });
        }
        output.write_all(framed.bytes())?;
    }
    if session.is_none() {
        return Err(Error::Invalid("video track has no packets"));
    }
    output.flush()?;
    Ok(())
}

fn write_recovered_avc(path: &Path, output: &mut impl Write) -> Result<()> {
    let mut file = File::open(path)?;
    let pool = Budget::new(64 << 20)?;
    let capabilities = seekable_capabilities(&mut file)?;
    let source_end = capabilities
        .length
        .ok_or(Error::Invalid("missing source length"))?;
    let mut input = InputCursor::new(&pool, SOURCE, capabilities, 65_536)?;
    let mut file = Feed::new(file, &pool)?;
    let mut opener = MovieOpen::new_recovery(&input, OpenLimits::default())?;
    loop {
        match opener.advance(&mut input, &mut WorkBudget::new(WORK), &CANCEL)? {
            OpenStep::NeedInput(request) => file.supply(&mut input, request)?,
            OpenStep::Ready => break,
            OpenStep::Yield | OpenStep::Root(_) => {}
        }
    }
    let movie = opener.finish(&input)?;
    let track = movie
        .tracks()
        .iter()
        .find(|track| {
            track.header().handler == *b"vide"
                && track
                    .table()
                    .descriptions()
                    .first()
                    .is_some_and(|description| [*b"avc1", *b"avc3"].contains(&description.format))
        })
        .ok_or(Error::Unsupported("no AVC video track"))?;
    let descriptions = track.table().descriptions();
    let mut cursor = track.table().cursor();
    let (raw, offset) = sample_configuration(descriptions, Codec::Avc, 1)?;
    let mut session = NalPacketSession::new(
        &pool,
        configuration(&pool, Codec::Avc, raw, offset)?,
        PACKET_LIMITS,
    )?;
    let mut description = 1;
    let mut written = 0usize;
    loop {
        let sample = match cursor.poll(&mut WorkBudget::new(WORK))? {
            SampleStep::Sample(sample) => sample,
            SampleStep::Yield => continue,
            SampleStep::End => break,
        };
        if sample.range.start >= source_end {
            break;
        }
        let attributes = (sample.description_index, sample.range.start);
        let media = movie
            .media()
            .iter()
            .find(|extent| sample.range.start >= extent.start && sample.range.end <= extent.end)
            .cloned()
            .ok_or(Error::Invalid("sample is outside media"))?;
        let mut reader = SampleReader::new_recovery(
            &mut input,
            sample,
            RecoverySampleExtent::Finite(media),
            PACKET_BYTES,
            RunRecoveryProfile,
        )?;
        let (bytes, short) = loop {
            match reader.poll_recovery(
                &mut input,
                &mut WorkBudget::new(WORK),
                &CANCEL,
                RunRecoveryProfile,
            )? {
                Step::NeedInput(request) => file.supply(&mut input, request)?,
                Step::Ready(RecoverySampleRead::Complete(packet)) => break (packet.payload, false),
                Step::Ready(RecoverySampleRead::Short(packet)) => break (packet.payload, true),
                Step::Yield => {}
                Step::End(_) => return Err(Error::Truncated),
            }
        };
        let (new_extradata, update) = if description == attributes.0 {
            (None, None)
        } else {
            let (raw, offset) = sample_configuration(descriptions, Codec::Avc, attributes.0)?;
            let update = avc_configuration(&pool, raw.clone(), offset, true)?;
            description = attributes.0;
            (Some(raw), Some(update))
        };
        let source = NalPacket {
            bytes,
            location: SourceLocation {
                source: SOURCE,
                offset: attributes.1,
            },
            generation: 0,
            fields: (),
            new_extradata,
        };
        let mut job = session.receiver_packet(source, update)?;
        match finish(|work| job.step(work, &CANCEL)) {
            Ok(framed) => {
                written = written
                    .checked_add(framed.bytes().len())
                    .ok_or(Error::Overflow)?;
                if written > OUTPUT_BYTES {
                    return Err(Error::Limit {
                        requested: written,
                        maximum: OUTPUT_BYTES,
                    });
                }
                output.write_all(framed.bytes())?;
            }
            Err(Error::Truncated) if short && job.take_receiver_rejection().is_some() => {}
            Err(error) => return Err(error),
        }
        if short {
            break;
        }
    }
    output.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let input = args
        .next()
        .ok_or(Error::Invalid("expected input and output paths"))?;
    let output = args
        .next()
        .ok_or(Error::Invalid("expected input and output paths"))?;
    let recovery = args.next();
    if args.next().is_some()
        || recovery
            .as_ref()
            .is_some_and(|value| value != "--recover-avc")
    {
        return Err(Error::Invalid("expected input and output paths"));
    }
    let output = File::options().write(true).create_new(true).open(output)?;
    let mut output = BufWriter::new(output);
    if recovery.is_some() {
        write_recovered_avc(Path::new(&input), &mut output)
    } else {
        write_annex_b(Path::new(&input), &mut output)
    }
}
