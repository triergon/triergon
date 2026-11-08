//! Helpers shared by the playback integration tests.
// Each test binary uses part of this file.
#![allow(dead_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, Result, SourceCapabilities, SourceId, SourceLocation, WorkBudget,
};
use triergon_framing::{
    AvcConfigLimits, AvcConfigParser, AvcConfigSource, BuildStep, HvcConfigLimits, HvcConfigParser,
    HvcConfigSource, NalPacket, NalPacketConfiguration, NalPacketLimits, NalPacketOutput,
    NalPacketSession,
};
use triergon_isobmff::{BoxStep, EntryLayout, Limits};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenProgress},
    *,
};

/// Steps a fixture is allowed before a test calls it stalled.
const STEPS: usize = 2_000_000;

/// A cancellation flag that was never raised.
pub fn off() -> AtomicBool {
    AtomicBool::new(false)
}

/// Advance an opener until it is ready to finish.
pub fn run_open<S: Source>(open: &mut Mp4Open<S>, units: u64) -> Result<()> {
    for _ in 0..STEPS {
        if matches!(
            open.advance(&mut WorkBudget::new(units), &off())?,
            OpenProgress::Ready
        ) {
            return Ok(());
        }
    }
    panic!("MP4 open did not make bounded progress");
}

/// An MP4 input served from memory, `quantum` bytes at a time.
pub fn byte_input(
    pool: &Budget,
    id: SourceId,
    raw: &[u8],
    quantum: usize,
    capabilities: SourceCapabilities,
    window: usize,
) -> Result<SelectedInput<ByteSource>> {
    let bytes = Bytes::copy_from_slice(pool, raw)?;
    Ok(SelectedInput {
        input: InputCursor::new(pool, id, capabilities, window)?,
        source: SourceOwner::new(ByteSource::new(id, bytes, quantum)?),
        format: Format::Mp4,
    })
}

/// Advance a player to its end one work unit at a time, appending what it delivers.
pub fn read<S: Source>(player: &mut Player<S>, out: &mut Vec<Output>) -> Result<()> {
    read_units(player, 1, out)
}

fn read_units<S: Source>(player: &mut Player<S>, units: u64, out: &mut Vec<Output>) -> Result<()> {
    for _ in 0..STEPS {
        let progress = player.advance(&mut WorkBudget::new(units), &off())?;
        while let Some(output) = player.pop() {
            out.push(output);
        }
        if matches!(progress, Progress::Ended) {
            return Ok(());
        }
    }
    panic!("bounded fixture stalled");
}

/// Advance a player to its end and return what it delivered.
pub fn collect<S: Source>(player: &mut Player<S>, units: u64) -> Result<Vec<Output>> {
    let mut out = Vec::new();
    read_units(player, units, &mut out)?;
    Ok(out)
}

/// The MP4 packets among delivered outputs.
pub fn packets(out: &[Output]) -> Vec<&Mp4Packet> {
    out.iter()
        .filter_map(|output| match output.event() {
            RawEvent::Mp4(Mp4Event::Packet(packet)) => Some(packet),
            _ => None,
        })
        .collect()
}

/// Frame the samples of an MP4 AVC or HEVC track as Annex B through
/// `triergon-framing`, starting from the sample entry's avcC or hvcC box.
pub fn annex_b_session(pool: &Budget, packet: &Mp4Packet) -> NalPacketSession {
    let entry = &packet.configuration;
    let mut children = entry
        .description
        .extensions(EntryLayout::Visual, Limits::default())
        .unwrap();
    let record = loop {
        match children.poll(&mut WorkBudget::new(1)).unwrap() {
            BoxStep::Box(raw) if matches!(&raw.header().kind, b"avcC" | b"hvcC") => break raw,
            BoxStep::Box(_) | BoxStep::Yield => {}
            BoxStep::End => panic!("sample entry has no avcC or hvcC"),
        }
    };
    let header = record.header();
    let bytes = record
        .bytes()
        .slice(usize::from(header.header_size)..record.bytes().len())
        .unwrap();
    let offset = header.start + u64::from(header.header_size);
    let (source, generation) = (packet.source.source, entry.revision);
    let configuration = if header.kind == *b"avcC" {
        let source = AvcConfigSource {
            source,
            generation,
            offset,
        };
        let limits = AvcConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_nals: 256,
            maximum_nal_bytes: 1 << 20,
        };
        let mut parser = AvcConfigParser::new(pool, source, bytes, limits).unwrap();
        NalPacketConfiguration::Avc(ready(|work| parser.step(work, &off()).unwrap()))
    } else {
        let source = HvcConfigSource {
            source,
            generation,
            offset,
        };
        let limits = HvcConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_arrays: 256,
            maximum_nals: 256,
            maximum_nal_bytes: 1 << 20,
        };
        let mut parser = HvcConfigParser::new(pool, source, bytes, limits).unwrap();
        NalPacketConfiguration::Hevc(ready(|work| parser.step(work, &off()).unwrap()))
    };
    let limits = NalPacketLimits {
        maximum_packet_bytes: 1 << 20,
        maximum_output_bytes: 2 << 20,
        maximum_nals: 1024,
        maximum_segments: 2048,
    };
    NalPacketSession::new(pool, configuration, limits).unwrap()
}

/// One MP4 sample framed as an Annex-B access unit.
pub fn annex_b(session: &mut NalPacketSession, packet: &Mp4Packet) -> NalPacketOutput<()> {
    let source = NalPacket {
        bytes: packet.source.payload.clone(),
        location: SourceLocation {
            source: packet.source.source,
            offset: packet.source.sample.range.start,
        },
        generation: packet.source.token.generation(),
        fields: (),
        new_extradata: None,
    };
    let mut job = session.packet(source, None).unwrap();
    ready(|work| job.step(work, &off()).unwrap())
}

/// Step a framing operation one work unit at a time until it publishes.
pub fn ready<T>(mut step: impl FnMut(&mut WorkBudget) -> BuildStep<T>) -> T {
    for _ in 0..STEPS {
        match step(&mut WorkBudget::new(1)) {
            BuildStep::Ready(output) => return output,
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("framing completed without output"),
        }
    }
    panic!("framing did not make bounded progress");
}
