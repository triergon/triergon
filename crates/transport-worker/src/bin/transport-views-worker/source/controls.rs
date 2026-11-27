//! Source-only facts that the public demux packet interface does not expose.

use std::collections::BTreeMap;

use triergon_core::container::{TrackId, TrackInfo};
use triergon_mpegts::psi::{PatTable, ProgramMap};
use triergon_mpegts::stream::{ActiveProgram, TraceEvent, TransportStream};
use triergon_mpegts::transport::{ContinuityOutcome, NullCarrierRun, TransportPacket};

use super::{Request, SourceRange, range_bytes};

#[derive(Default)]
pub(super) struct SourceControls {
    carrier: Option<CarrierFlags>,
    continuity: BTreeMap<u16, (CarrierFlags, bool)>,
    clock: u64,
}

#[derive(Clone, Copy)]
struct CarrierFlags {
    pid: u16,
    counter: u8,
    damaged: bool,
    discontinuity: bool,
    payload: bool,
    pcr: bool,
    range: SourceRange,
}

impl CarrierFlags {
    fn new(packet: &TransportPacket, range: SourceRange) -> Self {
        Self {
            pid: packet.pid,
            counter: packet.continuity_counter,
            damaged: packet.transport_error || packet.scrambling != 0,
            discontinuity: packet.discontinuity_indicator(),
            payload: packet
                .payload
                .as_ref()
                .is_some_and(|payload| !payload.is_empty()),
            pcr: packet
                .adaptation
                .as_ref()
                .is_some_and(|field| field.pcr.is_some()),
            range,
        }
    }
}

impl SourceControls {
    pub(super) fn check(
        &mut self,
        request: &Request,
        stream: &TransportStream,
        event: &TraceEvent,
    ) -> Result<(), String> {
        match event {
            TraceEvent::Transport(packet) => {
                self.carrier = Some(CarrierFlags::new(packet, carrier(request, packet)?));
            }
            TraceEvent::Continuity { pid, outcome } => {
                let current = self
                    .carrier
                    .filter(|packet| packet.pid == *pid)
                    .ok_or("continuity event has no preceding carrier on its PID")?;
                continuity(request, current, *outcome, &mut self.continuity)?;
            }
            TraceEvent::Section(section) => {
                ranges(
                    request,
                    section.bytes(),
                    section.source_ranges().iter().copied(),
                )?;
            }
            TraceEvent::Pat(table) => pat(request, table)?,
            TraceEvent::ProgramMap {
                map,
                active,
                configuration_epoch,
            } => {
                ranges(
                    request,
                    &map.source,
                    map.source_ranges.as_slice().iter().copied(),
                )?;
                if *active != map.header.current_next
                    || *configuration_epoch != stream.source_epochs().configuration
                {
                    return Err(
                        "PMT activation or configuration epoch differs from its source".into(),
                    );
                }
                if *active {
                    program(request, stream, map)?;
                }
            }
            TraceEvent::Timeline {
                program,
                clock_epoch,
            } => {
                let pcr = stream.active_program().and_then(ActiveProgram::pcr_pid);
                if *program != request.program
                    || *clock_epoch != self.clock + 1
                    || !self.carrier.is_some_and(|packet| {
                        pcr == Some(packet.pid) && packet.discontinuity && packet.pcr
                    })
                {
                    return Err("timeline did not follow the selected discontinuous PCR".into());
                }
                self.clock = *clock_epoch;
            }
            TraceEvent::Discontinuity { epochs } => {
                if stream.active_program().is_some() || epochs.clock != self.clock + 1 {
                    return Err(
                        "entity discontinuity left an active program or an old clock".into(),
                    );
                }
                self.clock = epochs.clock;
                self.carrier = None;
                self.continuity.clear();
            }
            TraceEvent::Diagnostic(fault) => {
                let entity = request
                    .entities
                    .get(super::source_index(fault.location.source))
                    .ok_or("diagnostic refers to an unknown entity")?;
                if fault.location.offset > entity.bytes.len() as u64 {
                    return Err("diagnostic lies outside the original entity".into());
                }
                if let Some(rejected) = &fault.rejected {
                    let range = rejected
                        .source_range()
                        .ok_or("rejected carrier lacks a range")?;
                    if range.entity != fault.location.source
                        || range.offset != fault.location.offset
                        || range_bytes(request, range) != Some(rejected.bytes().as_ref())
                    {
                        return Err("diagnostic carrier differs from its original bytes".into());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn ranges(
    request: &Request,
    mut section: &[u8],
    ranges: impl Iterator<Item = SourceRange>,
) -> Result<(), String> {
    for range in ranges {
        section = section
            .strip_prefix(
                range_bytes(request, range).ok_or("PSI source range lies outside its entity")?,
            )
            .ok_or("PSI section differs from its carrier bytes")?;
    }
    if !section.is_empty() {
        return Err("PSI ranges omit part of the section".into());
    }
    Ok(())
}

fn pat(request: &Request, table: &PatTable) -> Result<(), String> {
    let mut programs = table.programs.as_slice().iter();
    for (index, raw) in table.source_sections.as_slice().iter().enumerate() {
        ranges(
            request,
            raw,
            table
                .source_ranges
                .as_slice()
                .iter()
                .filter(|range| usize::from(range.section_number) == index)
                .map(|range| range.range),
        )?;
        if usize::from(raw[6]) != index
            || raw[7] != table.header.last_section_number
            || raw[5] & 1 == 0
            || (raw[5] >> 1) & 31 != table.header.version
            || u16::from_be_bytes([raw[3], raw[4]]) != table.header.extension
        {
            return Err("PAT section order or header differs from its source".into());
        }
        for entry in raw[8..raw.len() - 4].chunks_exact(4) {
            let program = programs.next().ok_or("PAT omitted a wire association")?;
            if program.program_number != u16::from_be_bytes([entry[0], entry[1]])
                || program.pid != u16::from_be_bytes([entry[2] & 31, entry[3]])
            {
                return Err("PAT association differs from its source bits".into());
            }
        }
    }
    if programs.next().is_some()
        || table.source_sections.len() != usize::from(table.header.last_section_number) + 1
    {
        return Err("PAT publication does not cover exactly the wire version".into());
    }
    Ok(())
}

fn program(request: &Request, stream: &TransportStream, map: &ProgramMap) -> Result<(), String> {
    let active = stream
        .active_program()
        .ok_or("active PMT has no borrowed view")?;
    let pcr = u16::from_be_bytes([map.source[8] & 31, map.source[9]]);
    let views = active.streams();
    let entries = map.streams.as_slice();
    if active.section().as_ref() != map.source.as_ref()
        || active.program_number() != request.program
        || active.version() != (map.source[5] >> 1) & 31
        || active.pcr_pid() != (pcr != 0x1fff).then_some(pcr)
        || views.len() != entries.len()
    {
        return Err("active program differs from the accepted wire PMT".into());
    }
    for (track, entry) in views.zip(entries) {
        if track.pid() != entry.pid
            || track.id() != TrackId(u64::from(entry.pid))
            || track.stream_type() != entry.stream_type
            || track.codec_tag() != [entry.stream_type]
            || track.descriptor_bytes().as_ref() != entry.descriptor_bytes.as_ref()
            || track.kind() != entry.kind()
            || track.codec() != TrackInfo::codec(entry)
            || !track.configuration().is_empty()
        {
            return Err("active stream differs from its PMT entry".into());
        }
        let mut language = None;
        let mut descriptors = track.descriptor_bytes().as_ref();
        for descriptor in track.descriptors() {
            let descriptor = descriptor.map_err(super::err)?;
            if !descriptors.starts_with(&[descriptor.tag, descriptor.data.len() as u8])
                || !descriptors[2..].starts_with(descriptor.data)
            {
                return Err("active stream descriptor differs from the PMT loop".into());
            }
            descriptors = &descriptors[2 + descriptor.data.len()..];
            if language.is_none() && descriptor.tag == 10 && descriptor.data.len() % 4 == 0 {
                language = descriptor.data.chunks_exact(4).find_map(|item| {
                    item[..3]
                        .iter()
                        .all(u8::is_ascii_alphabetic)
                        .then(|| std::str::from_utf8(&item[..3]).ok())
                        .flatten()
                });
            }
        }
        if !descriptors.is_empty() || track.language() != language {
            return Err("active stream omitted a PMT descriptor or its first language".into());
        }
    }
    Ok(())
}

fn carrier(request: &Request, packet: &TransportPacket) -> Result<SourceRange, String> {
    let bytes = request.format.bytes();
    let range = SourceRange {
        entity: packet.source_id.ok_or("source carrier has no entity")?,
        offset: packet.source_offset,
        bytes,
    };
    let raw = range_bytes(request, range).ok_or("carrier goes beyond its original entity")?;
    let ts = &raw[bytes - 188..];
    if raw != packet.source.as_ref()
        || ts[0] != 0x47
        || packet.pid != u16::from_be_bytes([ts[1] & 31, ts[2]])
        || packet.continuity_counter != ts[3] & 15
        || packet.payload_unit_start != (ts[1] & 64 != 0)
        || packet.transport_error != (ts[1] & 128 != 0)
        || packet.scrambling != ts[3] >> 6
    {
        return Err("source carrier differs from its original header or bytes".into());
    }
    Ok(range)
}

fn continuity(
    request: &Request,
    packet: CarrierFlags,
    outcome: ContinuityOutcome,
    history: &mut BTreeMap<u16, (CarrierFlags, bool)>,
) -> Result<(), String> {
    let pid = packet.pid;
    let previous = history.get(&pid).copied();
    let next = previous.map(|(prior, _)| (prior.counter + u8::from(packet.payload)) & 15);
    let duplicate = previous.is_some_and(|(prior, _)| {
        prior.payload
            && packet.payload
            && prior.counter == packet.counter
            && copies(request, prior, packet)
    });
    let valid = match outcome {
        ContinuityOutcome::Null => pid == 0x1fff,
        ContinuityOutcome::Lost { expected, actual } => {
            next == Some(expected)
                && actual == packet.counter
                && expected != actual
                && !packet.discontinuity
                && !duplicate
        }
        ContinuityOutcome::SignalledDiscontinuity => {
            packet.discontinuity && !duplicate && next.is_some_and(|next| next != packet.counter)
        }
        ContinuityOutcome::Damaged => packet.damaged,
        ContinuityOutcome::Duplicate => duplicate && previous.is_some_and(|(_, seen)| !seen),
        ContinuityOutcome::ExcessDuplicate => duplicate && previous.is_some_and(|(_, seen)| seen),
        ContinuityOutcome::First => pid != 0x1fff && !packet.damaged,
        ContinuityOutcome::Continuous => !packet.damaged && next == Some(packet.counter),
    };
    if !valid {
        return Err(format!(
            "continuity {outcome:?} contradicts the original carrier on PID {pid} at {}",
            packet.range.offset
        ));
    }
    match outcome {
        ContinuityOutcome::Null | ContinuityOutcome::ExcessDuplicate => {}
        ContinuityOutcome::Damaged => {
            history.remove(&pid);
        }
        ContinuityOutcome::Duplicate => {
            if let Some(entry) = history.get_mut(&pid) {
                entry.1 = true;
            }
        }
        _ => {
            history.insert(pid, (packet, false));
        }
    }
    Ok(())
}

fn copies(request: &Request, prior: CarrierFlags, current: CarrierFlags) -> bool {
    let transport = |range| range_bytes(request, range).map(|raw| &raw[raw.len() - 188..]);
    let (Some(a), Some(b)) = (transport(prior.range), transport(current.range)) else {
        return false;
    };
    if prior.pcr && current.pcr {
        a[..6] == b[..6] && a[12..] == b[12..]
    } else {
        a == b
    }
}

pub(super) fn null_run(request: &Request, run: &NullCarrierRun) -> Result<(), String> {
    if run.format() != request.format
        || run.is_empty()
        || run.bytes().len() != run.len() * request.format.bytes()
        || run.packet(run.len()).is_ok()
    {
        return Err("compact null run has an incorrect extent".into());
    }
    for index in 0..run.len() {
        let packet = run.packet(index).map_err(super::err)?;
        carrier(request, &packet)?;
        if packet.pid != 0x1fff
            || packet.source_id != Some(run.start().source)
            || packet.source_offset != run.start().offset + (index * request.format.bytes()) as u64
        {
            return Err("compact null run changed the source order or PID".into());
        }
    }
    Ok(())
}
