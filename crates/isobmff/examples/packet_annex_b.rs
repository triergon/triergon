//! Write one AVC or HEVC track as Annex B for byte-level comparisons.
use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, Bytes, Error, Result, SourceId, SourceLocation, WorkBudget,
    blocking::{Blocking, seekable_capabilities},
    container::{CodecId, Event, PacketInfo, TrackInfo},
};
use triergon_framing::{
    AvcConfigLimits, AvcConfigSource, BuildStep, Codec, HvcConfigLimits, HvcConfigSource,
    NalPacket, NalPacketConfiguration, NalPacketLimits, NalPacketSession,
    compat::nal::{ReceiverAvcConfigParser, ReceiverHevcConfigParser},
};
use triergon_isobmff::{BoxStep, EntryLayout, Limits, Reader, ReaderEvent, ReaderLimits, Track};

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

fn sample_configuration(track: &Track, codec: Codec, index: u32) -> Result<(Bytes, u64)> {
    let entry = track
        .descriptions()
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
            let (raw, offset) = sample_configuration(track, codec, sample.description_index)?;
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

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let input = args
        .next()
        .ok_or(Error::Invalid("expected input and output paths"))?;
    let output = args
        .next()
        .ok_or(Error::Invalid("expected input and output paths"))?;
    if args.next().is_some() {
        return Err(Error::Invalid("expected input and output paths"));
    }
    let output = File::options().write(true).create_new(true).open(output)?;
    write_annex_b(Path::new(&input), &mut BufWriter::new(output))
}
