//! Print the elementary streams and then the PES packets of an MPEG-TS file.
//!
//! ```text
//! cargo run -p triergon-mpegts --example mpegts_packets -- FILE [PROGRAM]
//! ```
//!
//! Without `PROGRAM` the file's PAT must list exactly one program. Streams are
//! printed when their PMT becomes active, PES packets as they complete, with raw
//! 33-bit timestamps on the 90 kHz clock. A PES carries no keyframe flag; `key`
//! is the random_access_indicator of the transport packet that starts it. The
//! file is read as 188-byte packets; use `PacketFormat::M2ts192` for M2TS.

use std::{collections::HashMap, env, fs::File, io::Read, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent, TransportStream, psi::DescriptorNamespace,
};

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args.next().ok_or("usage: mpegts_packets FILE [PROGRAM]")?;
    let program = args.next().map(|p| p.parse::<u16>()).transpose()?;
    let mut file = File::open(path)?;
    let caps = SourceCapabilities {
        length: Some(file.metadata()?.len()),
        random_access: false,
    };
    let pool = Budget::new(64 << 20)?;
    let mut input = InputCursor::new(&pool, SourceId(1), caps, 64 * 1024)?;
    let limits = ParseLimits::default();
    let mut ts = match program {
        Some(program) => TransportStream::new(pool.clone(), PacketFormat::Ts188, limits, program)?,
        None => TransportStream::new_single_program_segment(
            pool.clone(),
            limits,
            DescriptorNamespace::Unspecified,
        )?,
    };
    let cancel = AtomicBool::new(false);
    let mut buffer = vec![0; 64 * 1024];
    let mut streams_epoch = None;
    // random_access_indicator of each PES-starting transport packet, by payload offset.
    let mut starts = HashMap::new();
    loop {
        match ts.poll(&mut input, &mut WorkBudget::new(1 << 16), &cancel)? {
            Step::NeedInput(request) => {
                let wanted = request.maximum.min(buffer.len());
                let read = file.read(&mut buffer[..wanted])?;
                if read == 0 {
                    input.finish(request.source, request.token)?;
                } else {
                    let bytes = Bytes::copy_from_slice(&pool, &buffer[..read])?;
                    input.provide_owned(InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        bytes,
                    )?)?;
                }
            }
            Step::Yield => {}
            Step::End(_) => ts.end_of_source()?, // this file is the whole stream
            Step::Ready(TraceEvent::Transport(packet)) if packet.payload_unit_start => {
                let key = packet
                    .adaptation
                    .as_ref()
                    .is_some_and(|a| a.random_access_indicator);
                starts.insert((packet.pid, packet.payload_source_offset), key);
            }
            Step::Ready(TraceEvent::ProgramMap {
                map,
                active: true,
                configuration_epoch,
            }) => {
                if streams_epoch != Some(configuration_epoch) {
                    streams_epoch = Some(configuration_epoch);
                    for stream in map.streams.as_slice() {
                        println!(
                            "track pid {}: stream_type 0x{:02x}",
                            stream.pid, stream.stream_type
                        );
                    }
                }
            }
            Step::Ready(TraceEvent::Pes(pes)) => {
                let start = pes
                    .header_source_ranges
                    .as_slice()
                    .first()
                    .map(|r| r.offset);
                let key = start.and_then(|offset| starts.remove(&(pes.pid, offset)));
                println!(
                    "track {} dts {} pts {} size {} key {}",
                    pes.pid,
                    optional(pes.header.dts_raw33),
                    optional(pes.header.pts_raw33),
                    pes.payload_len,
                    key.unwrap_or(false)
                );
            }
            Step::Ready(TraceEvent::Diagnostic(fault)) => {
                eprintln!(
                    "{:?} fault on pid {:?}: {}",
                    fault.kind, fault.pid, fault.error
                );
            }
            Step::Ready(TraceEvent::Finished) => break,
            Step::Ready(_) => {} // other transport packets, sections, PAT, timeline
        }
    }
    Ok(())
}
