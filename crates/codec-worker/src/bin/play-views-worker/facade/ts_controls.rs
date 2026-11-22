//! Original carrier, continuity, PSI and PES records from the neutral facade.

use std::io::{Cursor, Write};

use triergon::mpegts::ReaderEvent;
use triergon::mpegts::stream::TraceEvent;
use triergon::mpegts::transport::ContinuityOutcome;
use triergon::{AnyOther, Blocking, Budget, Event};
use triergon_harness_protocol::hex;

use super::{Operation, Work, reader, transport_size};

fn row(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    fields: &[u64],
    bytes: &[u8],
) -> Result<(), String> {
    work.record(kind, fields, bytes);
    let Some(out) = trace else {
        return Ok(());
    };
    let text = (|| -> std::io::Result<()> {
        write!(out, "{kind}")?;
        for field in fields {
            write!(out, " {field}")?;
        }
        if !bytes.is_empty() {
            write!(out, " {}", hex(bytes))?;
        }
        writeln!(out)
    })();
    text.map_err(|error| error.to_string())
}

fn continuity(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    pid: u16,
    outcome: ContinuityOutcome,
) -> Result<(), String> {
    let pid = u64::from(pid);
    let code = match outcome {
        ContinuityOutcome::First => 0,
        ContinuityOutcome::Continuous => 1,
        ContinuityOutcome::Duplicate => 2,
        ContinuityOutcome::ExcessDuplicate => 3,
        ContinuityOutcome::SignalledDiscontinuity => 4,
        ContinuityOutcome::Lost { expected, actual } => {
            return row(
                work,
                trace,
                2,
                &[pid, 5, u64::from(expected), u64::from(actual)],
                &[],
            );
        }
        ContinuityOutcome::Damaged => 6,
        ContinuityOutcome::Null => 7,
    };
    row(work, trace, 2, &[pid, code], &[])
}

fn source_event(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    event: &TraceEvent,
) -> Result<(), String> {
    match event {
        TraceEvent::Continuity { pid, outcome } => continuity(work, trace, *pid, *outcome),
        TraceEvent::Section(section) => row(
            work,
            trace,
            3,
            &[
                u64::from(section.pid()),
                u64::from(section.crc_remainder().ok_or("PSI control has no CRC")?),
            ],
            section.bytes(),
        ),
        TraceEvent::Pat(table) => {
            let fields: Vec<_> = std::iter::once(u64::from(table.header.extension))
                .chain(table.programs.as_slice().iter().flat_map(|program| {
                    [u64::from(program.program_number), u64::from(program.pid)]
                }))
                .collect();
            row(work, trace, 4, &fields, &[])
        }
        TraceEvent::ProgramMap {
            map,
            active,
            configuration_epoch,
        } => {
            let fields: Vec<_> = [
                u64::from(map.header.extension),
                u64::from(map.pcr_pid),
                u64::from(*active),
                *configuration_epoch,
            ]
            .into_iter()
            .chain(
                map.streams
                    .as_slice()
                    .iter()
                    .flat_map(|stream| [u64::from(stream.stream_type), u64::from(stream.pid)]),
            )
            .collect();
            row(work, trace, 5, &fields, &map.source)
        }
        TraceEvent::Pes(pes) => {
            let payload: Vec<_> = pes.payload().flatten().copied().collect();
            row(work, trace, 6, &[u64::from(pes.pid)], pes.header.as_bytes())?;
            row(
                work,
                trace,
                9,
                &[u64::from(pes.pid), pes.elementary_offset],
                &payload,
            )
        }
        TraceEvent::Finished => row(work, trace, 7, &[], &[]),
        other => Err(format!(
            "unexpected source event in transport control: {other:?}"
        )),
    }
}

pub(super) fn prepare<'a>(input: &'a [u8], size: &str) -> Result<Operation<'a>, String> {
    let (_, format) = transport_size(size)?;
    let pool = Budget::new(64 << 20).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "facade-mpegts-controls",
        run: Box::new(move |work, mut trace| {
            let reader = reader(input, &pool, format)?;
            let mut driver = Blocking::seekable(reader, Cursor::new(input), &pool);
            while let Some(event) = driver.next_event().map_err(|error| error.to_string())? {
                match event {
                    Event::Other(AnyOther::MpegTs(ReaderEvent::Transport(packet))) => row(
                        work,
                        &mut trace,
                        1,
                        &[
                            packet
                                .source_id
                                .ok_or("facade carrier lacks a source identity")?
                                .0,
                            packet.source_offset,
                            u64::from(packet.pid),
                            u64::from(packet.continuity_counter),
                            u64::from(packet.payload_unit_start),
                            packet.payload_source_offset,
                        ],
                        &packet.source,
                    )?,
                    Event::Other(AnyOther::MpegTs(ReaderEvent::Continuity { pid, outcome })) => {
                        continuity(work, &mut trace, pid, outcome)?;
                    }
                    Event::Other(AnyOther::MpegTs(ReaderEvent::Source(event))) => {
                        source_event(work, &mut trace, event.event())?;
                    }
                    Event::Packet(_) => {}
                    other => return Err(format!("unexpected transport control: {other:?}")),
                }
            }
            Ok(())
        }),
    })
}
