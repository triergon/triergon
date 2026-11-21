//! Physical restart evidence for a deliberately small independently framed profile.
//!
//! H.222.0 §§2.4.3.3, 2.4.3.6–7 and 2.4.4: a PUSI carrier starts a PES,
//! PAT/PMT bind its PID, and PES source ranges identify its physical bytes.
//! Neither a timestamp nor a concatenated elementary offset supplies that proof.
//! Layer I/II headers delimit frames without Layer III's bit reservoir.
mod video;

use super::{Landing, Reader, ReaderEvent, Shared};
use crate::reader::{AccessUnit, Packet, SourceEvent};
use crate::{ContinuityOutcome, SourceEpochs, TraceEvent, TransportPacket};
use triergon_core::container::{Event, PacketInfo, ReaderStep};
use triergon_core::{
    Budget, BudgetVec, Charged, Error, Result, SourceCapabilities, SourceId, Step,
};
use triergon_framing::MpegLayer;

const WIDTH: usize = 188;
// Video publication can look ahead into the next PES. Keep two bounded PES
// extents and their tables; anything older or larger simply loses direct proof.
const VIDEO_CARRIERS: usize = 32;
const HISTORY: usize = 2 * (VIDEO_CARRIERS + 2);

#[derive(Clone, Copy)]
struct Carrier {
    offset: u64,
    pid: u16,
    payload: usize,
    pusi: bool,
    bytes: [u8; WIDTH],
}

impl Carrier {
    fn from_packet(packet: &TransportPacket) -> Option<Self> {
        let bytes: [u8; WIDTH] = packet.source.as_ref().try_into().ok()?;
        if packet.transport_error || packet.scrambling != 0 || packet.discontinuity_indicator() {
            return None;
        }
        Some(Self {
            offset: packet.source_offset,
            pid: packet.pid,
            payload: usize::try_from(
                packet
                    .payload_source_offset
                    .checked_sub(packet.source_offset)?,
            )
            .ok()?,
            pusi: packet.payload_unit_start,
            bytes,
        })
    }

    fn table(&self, pid: u16, section: &[u8]) -> bool {
        let Some(payload) = self.bytes.get(self.payload..) else {
            return false;
        };
        self.pid == pid
            && self.pusi
            && payload.first() == Some(&0)
            && payload.get(1..1 + section.len()) == Some(section)
            && payload
                .get(1 + section.len()..)
                .is_some_and(|tail| tail.iter().all(|byte| *byte == 0xff))
    }
}

struct Checkpoint {
    carriers: BudgetVec<Carrier>,
    elementary: u64,
    frames: usize,
    nals: usize,
}

pub(super) struct Index {
    source: SourceId,
    length: u64,
    pat: SourceEvent,
    map: SourceEvent,
    checkpoints: BudgetVec<Checkpoint>,
}

pub(super) struct Capture {
    source: SourceId,
    length: u64,
    next: u64,
    elementary: u64,
    frames: usize,
    nals: usize,
    pes: usize,
    maximum: usize,
    valid: bool,
    carriers: [Option<Carrier>; HISTORY],
    pat: Option<SourceEvent>,
    map: Option<SourceEvent>,
    checkpoints: BudgetVec<Checkpoint>,
}

impl Capture {
    pub(super) fn usable(&self) -> bool {
        self.valid
    }

    pub(super) fn new(
        pool: &Budget,
        source: SourceId,
        length: u64,
        maximum: usize,
    ) -> Result<Charged<Box<Self>>> {
        let capacity =
            maximum.min(usize::try_from(length / (3 * WIDTH) as u64).map_err(|_| Error::Overflow)?);
        Charged::boxed(
            pool,
            Self {
                source,
                length,
                maximum: capacity,
                next: 0,
                elementary: 0,
                frames: 0,
                nals: 0,
                pes: 0,
                valid: true,
                carriers: [None; HISTORY],
                pat: None,
                map: None,
                checkpoints: BudgetVec::new(pool),
            },
        )
    }

    pub(super) fn other(&mut self, event: &ReaderEvent) {
        if !self.valid {
            return;
        }
        match event {
            ReaderEvent::Transport(packet) => {
                if packet.prefix.is_some()
                    || packet.source_id != Some(self.source)
                    || packet.source_offset != self.next
                {
                    self.valid = false;
                    return;
                }
                let Some(carrier) = Carrier::from_packet(packet) else {
                    self.valid = false;
                    return;
                };
                let slot = (self.next / WIDTH as u64 % HISTORY as u64) as usize;
                self.carriers[slot] = Some(carrier);
                self.next += WIDTH as u64;
                if packet.pid != 0 && packet.pid != 0x1fff
                    && !self.map.as_ref().is_some_and(|map| {
                        map.map().streams.as_slice()[0].pid == packet.pid
                    })
                    && !self.pat.as_ref().is_some_and(|pat| {
                        matches!(pat.event(), TraceEvent::Pat(table) if table.programs.as_slice()[0].pid == packet.pid)
                    })
                { self.valid = false; }
            }
            ReaderEvent::Continuity { .. } => (),
            ReaderEvent::Source(source) => match source.event() {
                TraceEvent::Pat(table)
                    if self.pat.is_none()
                        && table.programs.len() == 1
                        && table.source_sections.len() == 1
                        && !table.programs.as_slice()[0].is_network_pid() =>
                {
                    self.pat = Some(source.clone());
                }
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch: 2,
                } if self.map.is_none()
                    && map.streams.len() == 1
                    && matches!(
                        map.streams.as_slice()[0].stream_type,
                        0x03 | 0x04 | 0x1b | 0x24
                    )
                    && (map.pcr_pid == 0x1fff || map.pcr_pid == map.streams.as_slice()[0].pid) =>
                {
                    self.map = Some(source.clone());
                }
                TraceEvent::Pes(_) => self.pes += 1,
                TraceEvent::Section(section) if section.crc_remainder() == Some(0) => (),
                TraceEvent::Continuity {
                    outcome: ContinuityOutcome::Null,
                    ..
                }
                | TraceEvent::Finished => (),
                _ => self.valid = false,
            },
            _ => self.valid = false,
        }
    }

    pub(super) fn packet(&mut self, packet: &Packet) {
        if !self.valid {
            return;
        }
        if !self.record_packet(packet) {
            self.valid = false;
        }
    }

    fn record_packet(&mut self, packet: &Packet) -> bool {
        let (maximum_carriers, nals) = match packet.access_unit() {
            AccessUnit::MpegAudio(frame)
                if matches!(frame.header().layer, MpegLayer::I | MpegLayer::II)
                    && frame.header().bitrate_kbps != 0
                    && frame.header().frame_bytes() == Some(packet.payload().len()) =>
            {
                (1, 0)
            }
            AccessUnit::Video(unit) => {
                let Some(nals) = video::nals(unit) else {
                    return false;
                };
                (VIDEO_CARRIERS, nals)
            }
            _ => return false,
        };
        let [origin] = packet.sources() else {
            return false;
        };
        let pes = origin.pes.pes();
        let [header] = pes.header_source_ranges.as_slice() else {
            return false;
        };
        let first = header.offset / WIDTH as u64 * WIDTH as u64;
        let Some(carrier) = self.carrier(first) else {
            return false;
        };
        let range = packet.elementary_range();
        if self.pes < self.frames + 1
            || pes.spans().is_empty()
            || pes.spans().len() > maximum_carriers
            || pes.header.declared_packet_length == 0
            || usize::from(pes.header.declared_packet_length) + 6
                != pes.header.as_bytes().len() + pes.payload_len
            || (nals != 0
                && (!pes.header.data_alignment_indicator
                    || !(0xe0..=0xef).contains(&pes.header.stream_id)))
            || packet.epochs()
                != (SourceEpochs {
                    configuration: 2,
                    clock: 0,
                })
            || pes.byte_run_epoch != 0
            || pes.elementary_offset != self.elementary
            || range.start != self.elementary
            || range.end.checked_sub(range.start) != Some(pes.payload_len as u64)
            || pes.payload_len != packet.payload().len()
            || origin.payload_range != (0..pes.payload_len)
            || header.entity != self.source
            || header.offset != carrier.offset + carrier.payload as u64
            || header.bytes != pes.header.as_bytes().len()
            || carrier
                .bytes
                .get(carrier.payload..carrier.payload + header.bytes)
                != Some(pes.header.as_bytes())
        {
            return false;
        }
        let mut consumed = 0;
        for (index, span) in pes.spans().iter().enumerate() {
            let Some(carrier) = self.carrier(first + (index * WIDTH) as u64) else {
                return false;
            };
            let physical = span.source_range();
            let start = carrier.payload + if index == 0 { header.bytes } else { 0 };
            let Some(bytes) = carrier.bytes.get(start..) else {
                return false;
            };
            if carrier.pid != pes.pid
                || carrier.pusi != (index == 0)
                || physical.entity != self.source
                || physical.offset != carrier.offset + start as u64
                || physical.bytes != bytes.len()
                || span.as_ref() != bytes
                || packet.payload().get(consumed..consumed + bytes.len()) != Some(bytes)
            {
                return false;
            }
            consumed += bytes.len();
        }
        if consumed != pes.payload_len {
            return false;
        }
        if packet.clock().and_then(|clock| clock.pts.exact).is_some()
            && let Some(pat) = first
                .checked_sub((2 * WIDTH) as u64)
                .and_then(|at| self.carrier(at))
            && let Some(pmt) = first
                .checked_sub(WIDTH as u64)
                .and_then(|at| self.carrier(at))
            && let (Some(pat_source), Some(map)) = (&self.pat, &self.map)
            && let TraceEvent::Pat(table) = pat_source.event()
            && pat.offset + WIDTH as u64 == pmt.offset
            && pmt.offset + WIDTH as u64 == first
            && pat.table(0, table.source_sections.as_slice()[0].as_ref())
            && pmt.table(table.programs.as_slice()[0].pid, map.map().source.as_ref())
        {
            if self.checkpoints.len() == self.maximum
                || (self.checkpoints.is_empty()
                    && self.checkpoints.try_reserve_exact(self.maximum).is_err())
            {
                return false;
            }
            let mut carriers = BudgetVec::new(self.checkpoints.budget());
            if carriers.try_reserve_exact(2 + pes.spans().len()).is_err()
                || carriers.try_push(pat).is_err()
                || carriers.try_push(pmt).is_err()
            {
                return false;
            }
            for index in 0..pes.spans().len() {
                let Some(carrier) = self.carrier(first + (index * WIDTH) as u64) else {
                    return false;
                };
                if carriers.try_push(carrier).is_err() {
                    return false;
                }
            }
            if self
                .checkpoints
                .try_push(Checkpoint {
                    carriers,
                    elementary: self.elementary,
                    frames: self.frames,
                    nals: self.nals,
                })
                .is_err()
            {
                return false;
            }
        }
        let Some(total) = self.nals.checked_add(nals) else {
            return false;
        };
        self.nals = total;
        self.elementary = range.end;
        self.frames += 1;
        true
    }

    fn carrier(&self, offset: u64) -> Option<Carrier> {
        let carrier = self.carriers[(offset / WIDTH as u64 % HISTORY as u64) as usize]?;
        (carrier.offset == offset).then_some(carrier)
    }

    pub(super) fn finish(mut capture: Charged<Box<Self>>, pool: &Budget) -> Option<Shared<Index>> {
        if !capture.valid
            || capture.next != capture.length
            || capture.pes != capture.frames
            || capture.checkpoints.is_empty()
        {
            return None;
        }
        let charge = Shared::<Index>::reserve(pool).ok()?;
        Some(Shared::new(
            Index {
                source: capture.source,
                length: capture.length,
                pat: capture.pat.take()?,
                map: capture.map.take()?,
                checkpoints: std::mem::replace(&mut capture.checkpoints, BudgetVec::new(pool)),
            },
            charge,
        ))
    }
}

pub(super) struct Direct {
    index: Shared<Index>,
    checkpoint: usize,
    carriers: usize,
    restored: bool,
}

impl Direct {
    pub(super) fn new(
        index: Option<&Shared<Index>>,
        landing: Landing,
        source: SourceId,
        caps: SourceCapabilities,
    ) -> Option<Self> {
        let index = index?;
        if index.source != source || caps.length != Some(index.length) || !caps.random_access {
            return None;
        }
        let checkpoint = index
            .checkpoints
            .as_slice()
            .binary_search_by_key(&landing.offset, |point| point.elementary)
            .ok()?;
        let result = Self {
            index: index.clone(),
            checkpoint,
            carriers: 0,
            restored: false,
        };
        (result.offset() != 0).then_some(result)
    }

    pub(super) fn offset(&self) -> u64 {
        self.index.checkpoints.as_slice()[self.checkpoint]
            .carriers
            .as_slice()[0]
            .offset
    }
    pub(super) fn verified(&self) -> bool {
        self.carriers
            == self.index.checkpoints.as_slice()[self.checkpoint]
                .carriers
                .len()
            && self.restored
    }

    pub(super) fn observe(
        &mut self,
        reader: &mut Reader,
        step: &mut ReaderStep<Reader>,
    ) -> Result<()> {
        let Step::Ready(Event::Other(event)) = step else {
            return Ok(());
        };
        match event {
            ReaderEvent::Transport(packet) => {
                let Some(expected) = self.index.checkpoints.as_slice()[self.checkpoint]
                    .carriers
                    .as_slice()
                    .get(self.carriers)
                else {
                    // The ordinary video framer can read the following PES to
                    // establish the landing picture's final NAL boundary.
                    return Ok(());
                };
                if packet.source_id != Some(self.index.source)
                    || packet.source_offset != expected.offset
                    || packet.source.as_ref() != expected.bytes
                {
                    return Err(Error::SourceMismatch);
                }
                self.carriers += 1;
            }
            ReaderEvent::Source(source) => match source.event() {
                TraceEvent::Pat(table) => {
                    let TraceEvent::Pat(original) = self.index.pat.event() else {
                        unreachable!("captured PAT")
                    };
                    if self.carriers != 1
                        || table.header != original.header
                        || table.source_sections.as_slice()[0].as_ref()
                            != original.source_sections.as_slice()[0].as_ref()
                    {
                        return Err(Error::SourceMismatch);
                    }
                    *source = self.index.pat.clone();
                }
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch: 2,
                } => {
                    if self.carriers != 2
                        || map.source.as_ref() != self.index.map.map().source.as_ref()
                        || reader.lanes.len() != 1
                    {
                        return Err(Error::SourceMismatch);
                    }
                    let checkpoint = &self.index.checkpoints.as_slice()[self.checkpoint];
                    let pid = self.index.map.map().streams.as_slice()[0].pid;
                    reader
                        .stream
                        .as_mut()
                        .ok_or(Error::SourceMismatch)?
                        .resume_elementary_offset(pid, checkpoint.elementary)?;
                    reader.lanes.as_mut_slice()[0].resume_independent(
                        checkpoint.elementary,
                        checkpoint.frames,
                        checkpoint.nals,
                        self.index.map.clone(),
                    )?;
                    reader.tracks.as_mut_slice()[0] = reader.lanes.as_slice()[0].track.clone();
                    reader.map = Some(self.index.map.clone());
                    reader.boundary = Some(self.index.map.clone());
                    *source = self.index.map.clone();
                    self.restored = true;
                }
                TraceEvent::Section(_) | TraceEvent::Pes(_) => (),
                _ => return Err(Error::SourceMismatch),
            },
            ReaderEvent::Continuity { .. } => (),
            _ => return Err(Error::SourceMismatch),
        }
        Ok(())
    }
}
