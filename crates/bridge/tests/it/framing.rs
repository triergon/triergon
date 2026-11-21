//! Annex B framing of MP4 samples through `triergon-framing`, the path [`AccessUnits`]
//! is checked against.
//!
//! [`AccessUnits`]: triergon_bridge::access::AccessUnits
use crate::support::off;
use triergon_core::{Budget, SourceLocation, WorkBudget};
use triergon_framing::{
    BuildStep,
    avc::{AvcConfigLimits, AvcConfigParser, AvcConfigSource},
    hevc::{HvcConfigLimits, HvcConfigParser, HvcConfigSource},
    nal::{NalPacket, NalPacketConfiguration, NalPacketLimits, NalPacketOutput, NalPacketSession},
};
use triergon_isobmff::{Limits, Next, movie::EntryLayout};
use triergon_playback::mp4::Mp4Packet;

/// Steps a fixture is allowed before a test calls it stalled.
const STEPS: usize = 2_000_000;

/// Frame the samples of an MP4 AVC or HEVC track as Annex B through
/// `triergon-framing`, starting from the sample entry's avcC or hvcC box.
pub(crate) fn annex_b_session(pool: &Budget, packet: &Mp4Packet) -> NalPacketSession {
    let entry = &packet.configuration;
    let mut children = entry
        .description
        .extensions(EntryLayout::Visual, Limits::default())
        .unwrap();
    let record = (0..STEPS)
        .find_map(|_| match children.poll(&mut WorkBudget::new(1)).unwrap() {
            Next::Item(raw) if matches!(&raw.header().kind, b"avcC" | b"hvcC") => Some(raw),
            Next::Item(_) | Next::Yield => None,
            Next::End => panic!("sample entry has no avcC or hvcC"),
        })
        .expect("sample entry walk did not make bounded progress");
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
pub(crate) fn annex_b(session: &mut NalPacketSession, packet: &Mp4Packet) -> NalPacketOutput<()> {
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
pub(crate) fn ready<T>(mut step: impl FnMut(&mut WorkBudget) -> BuildStep<T>) -> T {
    for _ in 0..STEPS {
        match step(&mut WorkBudget::new(1)) {
            BuildStep::Ready(output) => return output,
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("framing completed without output"),
        }
    }
    panic!("framing did not make bounded progress");
}
