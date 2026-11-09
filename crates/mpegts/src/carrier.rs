//! Owned strict transport inspection. Work advances actual bounded fields/bytes.
use crate::{Adaptation, AssemblyStep, PacketFormat, Pcr, SourceRange, TransportPacket};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, PushError, Result, SourceId, SourceLocation, WorkBudget,
};

const STUFFING: [u8; 182] = [0xff; 182];
pub(crate) const TS188_BYTES: usize = 188;
pub(crate) const TS_PAYLOAD_START: usize = 4;
pub(crate) const PLAIN_PAYLOAD_BYTES: usize = TS188_BYTES - TS_PAYLOAD_START;

/// Header of a clear, payload-only continuation in a complete TS188 carrier.
pub(crate) fn payload_only_continuation(ts: &[u8]) -> Option<(u16, u8)> {
    let [0x47, high, low, control, ..] = ts else {
        return None;
    };
    if high & 0xc0 != 0 || control & 0xf0 != 0x10 {
        return None;
    }
    let pid = (u16::from(high & 31) << 8) | u16::from(*low);
    Some((pid, control & 15))
}

#[derive(Debug)]
struct CarrierHeader {
    pid: u16,
    pusi: bool,
    priority: bool,
    tei: bool,
    scrambling: u8,
    cc: u8,
    afc: u8,
}

impl CarrierHeader {
    fn parse(data: &[u8]) -> Result<Self> {
        let [0x47, high, low, control, ..] = data else {
            return Err(Error::Invalid("TS sync byte"));
        };
        Ok(Self {
            pid: (u16::from(high & 31) << 8) | u16::from(*low),
            pusi: high & 64 != 0,
            priority: high & 32 != 0,
            tei: high & 128 != 0,
            scrambling: control >> 6,
            cc: control & 15,
            afc: (control >> 4) & 3,
        })
    }
}

#[derive(Debug)]
pub(crate) struct PlainCarrier {
    header: CarrierHeader,
    payload_start: usize,
    flags: Option<u8>,
    pcr: Option<Pcr>,
}

impl PlainCarrier {
    /// Validate payload-only carriers and adaptations containing flags, an
    /// optional PCR and stuffing. Charge the incremental path's fields/bytes.
    pub(crate) fn inspect_ts188(
        source: &Bytes,
        source_offset: u64,
        work: &mut WorkBudget,
    ) -> Option<Self> {
        let data = source.as_ref();
        if data.len() != TS188_BYTES || source_offset.checked_add(TS188_BYTES as u64).is_none() {
            return None;
        }
        let header = CarrierHeader::parse(data).ok()?;
        let (payload_start, flags, pcr, charge) = match header.afc {
            1 => (4, None, None, 2),
            3 if data[4] == 0 => (5, Some(0), None, 4),
            3 if data[4] <= 182 && data[5] & 15 == 0 => {
                let end = 5 + usize::from(data[4]);
                let has_pcr = data[5] & 0x10 != 0;
                let stuffing_start = if has_pcr { 12 } else { 6 };
                let stuffing = end.checked_sub(stuffing_start)?;
                let charge = 10 + stuffing as u64;
                if work.remaining() < charge {
                    return None;
                }
                let pcr = if has_pcr {
                    Some(crate::transport::pcr(&data[6..12]).ok()?)
                } else {
                    None
                };
                if data[stuffing_start..end] != STUFFING[..stuffing] {
                    return None;
                }
                (end, Some(data[5]), pcr, charge)
            }
            _ => return None,
        };
        work.try_charge(charge).then_some(Self {
            header,
            payload_start,
            flags,
            pcr,
        })
    }

    pub(crate) fn is_plain_continuation(&self) -> bool {
        !self.header.pusi
            && !self.header.tei
            && self.header.scrambling == 0
            && self.flags.unwrap_or(0) & 0x80 == 0
            && self.pcr.is_none()
    }

    pub(crate) fn into_packet(
        self,
        source: Bytes,
        source_offset: u64,
        source_id: Option<SourceId>,
    ) -> Result<TransportPacket> {
        let payload = source.slice(self.payload_start..188)?;
        let adaptation = if let Some(flags) = self.flags {
            Some(Adaptation {
                source: source.slice(4..self.payload_start)?,
                discontinuity: flags & 128 != 0,
                random_access_indicator: flags & 64 != 0,
                elementary_priority: flags & 32 != 0,
                pcr: self.pcr,
                opcr: None,
                splice_countdown: None,
                private_data: None,
                extension: None,
            })
        } else {
            None
        };
        Ok(TransportPacket {
            source,
            source_offset,
            source_id,
            prefix: None,
            pid: self.header.pid,
            payload_unit_start: self.header.pusi,
            transport_priority: self.header.priority,
            transport_error: self.header.tei,
            scrambling: self.header.scrambling,
            continuity_counter: self.header.cc,
            adaptation,
            payload: Some(payload),
            payload_source_offset: source_offset
                .checked_add(self.payload_start as u64)
                .ok_or(Error::Overflow)?,
            sync: 0,
            pcr_range: self.pcr.map(|_| 6..12),
        })
    }
}

/// A complete plain TS188 continuation with its charged backing storage.
#[derive(Debug)]
pub(crate) struct PlainContinuation {
    pub(crate) layout: PlainCarrier,
    pub(crate) source: Option<Bytes>,
    pub(crate) offset: u64,
    pub(crate) entity: Option<SourceId>,
}

impl PlainContinuation {
    pub(crate) fn pid(&self) -> u16 {
        self.layout.header.pid
    }

    pub(crate) fn counter(&self) -> u8 {
        self.layout.header.cc
    }

    pub(crate) fn payload_start(&self) -> usize {
        self.layout.payload_start
    }

    pub(crate) fn into_packet(self) -> Result<TransportPacket> {
        self.layout.into_packet(
            self.source
                .ok_or(Error::Invalid("missing continuation source"))?,
            self.offset,
            self.entity,
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Envelope {
    pub pid: u16,
    pub pusi: bool,
    pub priority: bool,
    pub tei: bool,
    pub scrambling: u8,
    pub cc: u8,
    pub afc: u8,
    pub flags: u8,
    pub payload_start: usize,
    adaptation_end: usize,
    pcr: Option<Pcr>,
    opcr: Option<Pcr>,
    pcr_range: Option<Range<usize>>,
    splice: Option<i8>,
    private: Option<Range<usize>>,
    extension: Option<Range<usize>>,
}

/// Opaque real rejection, including the entire original hidden storage owner.
#[derive(Clone, Debug)]
pub struct RejectedTransport {
    bytes: Bytes,
    format: PacketFormat,
    offset: u64,
    entity: Option<SourceId>,
    error: Error,
    secondary: Option<Error>,
    complete_check: bool,
    envelope: Envelope,
}
impl RejectedTransport {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn format(&self) -> PacketFormat {
        self.format
    }
    pub fn source_offset(&self) -> u64 {
        self.offset
    }
    pub fn source_id(&self) -> Option<SourceId> {
        self.entity
    }
    pub fn error(&self) -> Error {
        self.error
    }
    pub fn secondary_error(&self) -> Option<Error> {
        self.secondary
    }
    pub fn fully_checked(&self) -> bool {
        self.complete_check
    }
    pub fn source_range(&self) -> Option<SourceRange> {
        self.entity.map(|entity| SourceRange {
            entity,
            offset: self.offset,
            bytes: self.bytes.len(),
        })
    }
    pub(crate) fn envelope(&self) -> &Envelope {
        &self.envelope
    }
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one inline owned carrier; all backing storage is already charged"
)]
pub enum TransportInspection {
    Accepted(TransportPacket),
    Rejected(RejectedTransport),
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Header,
    Length,
    Flags,
    Pcr,
    Opcr,
    Splice,
    Private,
    Extension,
    Stuffing,
    Finish,
}

/// One <=192-byte input. Fixed header/clock actions inspect at most six bytes;
/// stuffing is scanned in bounded runs with one unit per inspected byte.
#[derive(Debug)]
pub struct TransportInspector {
    bytes: Option<Bytes>,
    format: PacketFormat,
    offset: u64,
    entity: Option<SourceId>,
    phase: Phase,
    position: usize,
    fields: Envelope,
    error: Option<Error>,
    secondary: Option<Error>,
    complete_check: bool,
}
impl TransportInspector {
    pub fn new(
        bytes: Bytes,
        format: PacketFormat,
        location: SourceLocation,
        budget: &Budget,
    ) -> std::result::Result<Self, PushError<Bytes>> {
        if !bytes.belongs_to(budget) {
            return Err(PushError {
                error: Error::ForeignPool,
                value: bytes,
            });
        }
        Self::detached(bytes, format, location.offset, Some(location.source))
    }
    pub(crate) fn detached(
        bytes: Bytes,
        format: PacketFormat,
        offset: u64,
        entity: Option<SourceId>,
    ) -> std::result::Result<Self, PushError<Bytes>> {
        let error = if bytes.len() != format.bytes() {
            Some(Error::Invalid("transport packet size"))
        } else if offset.checked_add(bytes.len() as u64).is_none() {
            Some(Error::Overflow)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(PushError {
                error,
                value: bytes,
            });
        }
        Ok(Self {
            bytes: Some(bytes),
            format,
            offset,
            entity,
            phase: Phase::Header,
            position: 0,
            fields: Envelope {
                pid: 0,
                pusi: false,
                priority: false,
                tei: false,
                scrambling: 0,
                cc: 0,
                afc: 0,
                flags: 0,
                payload_start: 4,
                adaptation_end: 4,
                pcr: None,
                opcr: None,
                pcr_range: None,
                splice: None,
                private: None,
                extension: None,
            },
            error: None,
            secondary: None,
            complete_check: false,
        })
    }
    pub fn backing_bytes(&self) -> Option<&Bytes> {
        self.bytes.as_ref()
    }
    fn fail(&mut self, error: Error) {
        if self.error.is_none() {
            self.error = Some(error);
        } else {
            self.secondary = Some(error);
        }
        self.phase = Phase::Finish;
    }
    fn field_end(position: usize, end: usize, count: usize) -> Result<usize> {
        let next = position.checked_add(count).ok_or(Error::Overflow)?;
        if next > end {
            Err(Error::Invalid("adaptation field length"))
        } else {
            Ok(next)
        }
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<TransportInspection>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.bytes.is_none() {
                return Ok(AssemblyStep::Finished);
            }
            if matches!(self.phase, Phase::Stuffing) && self.position < self.fields.adaptation_end {
                // The carrier stays owned by the inspector. Inspect only the
                // funded bytes and stop charging at the first syntax error.
                let available = (self.fields.adaptation_end - self.position).min(64) as u64;
                let count = available.min(work.remaining()) as usize;
                if count == 0 {
                    return Ok(AssemblyStep::Yield);
                }
                let prefix = self.format.prefix_bytes();
                let start = prefix + self.position;
                let source = self
                    .bytes
                    .as_ref()
                    .ok_or(Error::Invalid("carrier owner missing"))?;
                let stuffing = &source.as_ref()[start..start + count];
                let invalid = (stuffing != &STUFFING[..count])
                    .then(|| stuffing.iter().position(|&byte| byte != 0xff))
                    .flatten();
                if let Some(invalid) = invalid {
                    work.take((invalid + 1) as u64);
                    self.position += invalid;
                    self.fail(Error::Invalid("adaptation stuffing"));
                } else {
                    work.take(count as u64);
                    self.position += count;
                }
                continue;
            }
            if !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            if matches!(self.phase, Phase::Finish) {
                return self.finish().map(AssemblyStep::Output);
            }
            // The following borrow is confined to one fixed field.
            let source = self
                .bytes
                .as_ref()
                .ok_or(Error::Invalid("carrier owner missing"))?;
            let data = &source.as_ref()[self.format.prefix_bytes()..];
            let action: Result<()> = (|| {
                match self.phase {
                    Phase::Header => {
                        let header = CarrierHeader::parse(data)?;
                        self.fields.pid = header.pid;
                        self.fields.pusi = header.pusi;
                        self.fields.priority = header.priority;
                        self.fields.tei = header.tei;
                        self.fields.scrambling = header.scrambling;
                        self.fields.cc = header.cc;
                        self.fields.afc = header.afc;
                        if self.fields.afc == 0 {
                            return Err(Error::Invalid("reserved adaptation control"));
                        }
                        if self.fields.afc & 2 == 0 {
                            self.complete_check = true;
                            self.phase = Phase::Finish;
                        } else {
                            self.phase = Phase::Length;
                        }
                    }
                    Phase::Length => {
                        let length = usize::from(data[4]);
                        if (self.fields.afc == 2 && length != 183)
                            || (self.fields.afc == 3 && length > 182)
                        {
                            return Err(Error::Invalid("adaptation length/control"));
                        }
                        self.fields.adaptation_end = 5 + length;
                        self.fields.payload_start = 5 + length;
                        self.position = if length == 0 { 5 } else { 6 };
                        self.phase = if length == 0 {
                            Phase::Stuffing
                        } else {
                            Phase::Flags
                        };
                    }
                    Phase::Flags => {
                        self.fields.flags = data[5];
                        // Continue the remaining real checks so this first semantic
                        // fault cannot conceal a second envelope/clock fault.
                        if data[5] & 0x18 == 0x08 {
                            self.error = Some(Error::Invalid("OPCR without PCR"));
                        }
                        self.phase = if self.fields.flags & 31 == 0 && work.try_charge(5) {
                            Phase::Stuffing
                        } else {
                            Phase::Pcr
                        };
                    }
                    Phase::Pcr => {
                        if self.fields.flags & 0x10 != 0 {
                            let next =
                                Self::field_end(self.position, self.fields.adaptation_end, 6)?;
                            match crate::transport::pcr(&data[self.position..next]) {
                                Ok(clock) => {
                                    self.fields.pcr = Some(clock);
                                    self.fields.pcr_range = Some(self.position..next);
                                }
                                Err(error) => {
                                    // Keep the strict first error; inspect subsequent real fields
                                    // and stuffing so a second fault cannot be concealed.
                                    if self.error.is_none() {
                                        self.error = Some(error);
                                    } else {
                                        self.secondary = Some(error);
                                    }
                                }
                            }
                            self.position = next;
                        }
                        self.phase = if self.fields.flags & 15 == 0 && work.try_charge(4) {
                            Phase::Stuffing
                        } else {
                            Phase::Opcr
                        };
                    }
                    Phase::Opcr => {
                        if self.fields.flags & 8 != 0 {
                            let next =
                                Self::field_end(self.position, self.fields.adaptation_end, 6)?;
                            self.fields.opcr =
                                Some(crate::transport::pcr(&data[self.position..next])?);
                            self.position = next;
                        }
                        self.phase = Phase::Splice;
                    }
                    Phase::Splice => {
                        if self.fields.flags & 4 != 0 {
                            let next =
                                Self::field_end(self.position, self.fields.adaptation_end, 1)?;
                            self.fields.splice = Some(data[self.position] as i8);
                            self.position = next;
                        }
                        self.phase = Phase::Private;
                    }
                    Phase::Private | Phase::Extension => {
                        let private = matches!(self.phase, Phase::Private);
                        let flag = if private { 2 } else { 1 };
                        if self.fields.flags & flag != 0 {
                            Self::field_end(self.position, self.fields.adaptation_end, 1)?;
                            let count = usize::from(data[self.position]);
                            let start = self.position + 1;
                            let next = start.checked_add(count).ok_or(Error::Overflow)?;
                            if next > self.fields.adaptation_end {
                                return Err(Error::Invalid("adaptation field length"));
                            }
                            if private {
                                self.fields.private = Some(start..next);
                            } else {
                                self.fields.extension = Some(start..next);
                            }
                            self.position = next;
                        }
                        self.phase = if private {
                            Phase::Extension
                        } else {
                            Phase::Stuffing
                        };
                    }
                    Phase::Stuffing => {
                        self.complete_check = true;
                        self.phase = Phase::Finish;
                    }
                    Phase::Finish => unreachable!(),
                }
                Ok(())
            })();
            if let Err(error) = action {
                self.fail(error);
            }
        }
    }
    fn finish(&mut self) -> Result<TransportInspection> {
        let source = self
            .bytes
            .take()
            .ok_or(Error::Invalid("carrier already delivered"))?;
        if let Some(error) = self.error {
            // Checked encoding is not an accepted source clock on a rejected
            // carrier. Keep original clock bytes, never expose a validated value.
            let mut envelope = self.fields.clone();
            envelope.pcr = None;
            envelope.opcr = None;
            return Ok(TransportInspection::Rejected(RejectedTransport {
                bytes: source,
                format: self.format,
                offset: self.offset,
                entity: self.entity,
                error,
                secondary: self.secondary,
                complete_check: self.complete_check,
                envelope,
            }));
        }
        let sync = self.format.prefix_bytes();
        let f = &self.fields;
        let adaptation = if f.afc & 2 != 0 {
            Some(Adaptation {
                source: source.slice(sync + 4..sync + f.adaptation_end)?,
                discontinuity: f.flags & 128 != 0,
                random_access_indicator: f.flags & 64 != 0,
                elementary_priority: f.flags & 32 != 0,
                pcr: f.pcr,
                opcr: f.opcr,
                splice_countdown: f.splice,
                private_data: f
                    .private
                    .as_ref()
                    .map(|r| source.slice(sync + r.start..sync + r.end))
                    .transpose()?,
                extension: f
                    .extension
                    .as_ref()
                    .map(|r| source.slice(sync + r.start..sync + r.end))
                    .transpose()?,
            })
        } else {
            None
        };
        let payload = if f.afc & 1 != 0 {
            Some(source.slice(sync + f.payload_start..source.len())?)
        } else {
            None
        };
        Ok(TransportInspection::Accepted(TransportPacket {
            prefix: if sync == 0 {
                None
            } else {
                Some(source.slice(0..sync)?)
            },
            source,
            source_offset: self.offset,
            source_id: self.entity,
            pid: f.pid,
            payload_unit_start: f.pusi,
            transport_priority: f.priority,
            transport_error: f.tei,
            scrambling: f.scrambling,
            continuity_counter: f.cc,
            adaptation,
            payload,
            payload_source_offset: self.offset + (sync + f.payload_start) as u64,
            sync,
            pcr_range: f.pcr_range.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcr_carrier(base: u64, extension: u16, stuffing: usize, flags: u8) -> [u8; 188] {
        let mut wire = [0x42; 188];
        wire[..6].copy_from_slice(&[
            0x47,
            1,
            7,
            0x37,
            (7 + stuffing).try_into().unwrap(),
            flags | 0x10,
        ]);
        wire[6..12].copy_from_slice(&[
            (base >> 25).try_into().unwrap(),
            ((base >> 17) & 255).try_into().unwrap(),
            ((base >> 9) & 255).try_into().unwrap(),
            ((base >> 1) & 255).try_into().unwrap(),
            0x7e | u8::try_from((base & 1) << 7).unwrap() | u8::try_from(extension >> 8).unwrap(),
            (extension & 255).try_into().unwrap(),
        ]);
        wire[12..12 + stuffing].fill(0xff);
        wire
    }

    fn incremental(bytes: Bytes, offset: u64, quantum: u64) -> (TransportInspection, u64) {
        let mut inspector =
            TransportInspector::detached(bytes, PacketFormat::Ts188, offset, Some(SourceId(7)))
                .unwrap();
        let mut spent = 0;
        loop {
            let mut work = WorkBudget::new(quantum);
            let step = inspector.poll(&mut work, &AtomicBool::new(false)).unwrap();
            spent += work.spent();
            match step {
                AssemblyStep::Output(result) => return (result, spent),
                AssemblyStep::Yield => {}
                _ => panic!("inspection did not produce a carrier"),
            }
        }
    }

    fn assert_same_packet(fast: &TransportPacket, slow: &TransportPacket) {
        assert_eq!(
            (
                fast.source_offset,
                fast.source_id,
                fast.pid,
                fast.payload_unit_start,
                fast.transport_priority,
                fast.transport_error,
                fast.scrambling,
                fast.continuity_counter
            ),
            (
                slow.source_offset,
                slow.source_id,
                slow.pid,
                slow.payload_unit_start,
                slow.transport_priority,
                slow.transport_error,
                slow.scrambling,
                slow.continuity_counter
            )
        );
        assert_eq!(
            (fast.sync, &fast.pcr_range, fast.payload_source_offset),
            (slow.sync, &slow.pcr_range, slow.payload_source_offset)
        );
        assert_eq!(fast.source.as_ref(), slow.source.as_ref());
        for (a, b) in [(&fast.prefix, &slow.prefix), (&fast.payload, &slow.payload)] {
            assert_eq!(a.as_ref().map(AsRef::as_ref), b.as_ref().map(AsRef::as_ref));
        }
        let (Some(a), Some(b)) = (&fast.adaptation, &slow.adaptation) else {
            panic!("expected adaptation on both carriers");
        };
        assert_eq!(
            (
                a.discontinuity,
                a.random_access_indicator,
                a.elementary_priority,
                a.pcr,
                a.opcr,
                a.splice_countdown
            ),
            (
                b.discontinuity,
                b.random_access_indicator,
                b.elementary_priority,
                b.pcr,
                b.opcr,
                b.splice_countdown
            )
        );
        assert_eq!(a.source.as_ref(), b.source.as_ref());
        for (a, b) in [
            (&a.private_data, &b.private_data),
            (&a.extension, &b.extension),
        ] {
            assert_eq!(a.as_ref().map(AsRef::as_ref), b.as_ref().map(AsRef::as_ref));
        }
    }

    fn assert_fallback(bytes: &Bytes, offset: u64) {
        for units in [0, 1, 9, 4096] {
            let mut work = WorkBudget::new(units);
            assert!(PlainCarrier::inspect_ts188(bytes, offset, &mut work).is_none());
            assert_eq!(work.spent(), 0);
        }
    }

    #[test]
    fn pcr_adaptation_matches_incremental_fields_ranges_bytes_and_work() {
        let pool = Budget::new(8192).unwrap();
        for (base, extension) in [
            (0, 0),
            (0, 299),
            (1, 1),
            (0x123456789, 257),
            ((1 << 33) - 1, 299),
        ] {
            for stuffing in [0, 1, 63, 64, 65, 175] {
                for flags in (0..=0xe0).step_by(0x20) {
                    for header in [0, 0x40, 0xa0] {
                        let mut wire = pcr_carrier(base, extension, stuffing, flags);
                        wire[1] |= header;
                        if header == 0xa0 {
                            wire[3] |= 0x80;
                        }
                        let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
                        for offset in [0, 188, u64::MAX - 188] {
                            let charge = 10 + stuffing as u64;
                            for units in [0, 1, charge - 1, charge, charge + 1] {
                                let mut work = WorkBudget::new(units);
                                let result = PlainCarrier::inspect_ts188(&bytes, offset, &mut work);
                                assert_eq!(result.is_some(), units >= charge);
                                assert_eq!(work.spent(), if units >= charge { charge } else { 0 });
                            }
                            let mut work = WorkBudget::new(charge);
                            let plain =
                                PlainCarrier::inspect_ts188(&bytes, offset, &mut work).unwrap();
                            assert!(!plain.is_plain_continuation());
                            let fast = plain
                                .into_packet(bytes.clone(), offset, Some(SourceId(7)))
                                .unwrap();
                            assert_eq!(
                                fast.adaptation.as_ref().unwrap().pcr,
                                Some(Pcr { base, extension })
                            );
                            assert_eq!(fast.pcr_range, Some(6..12));
                            for quantum in [1, 4, 9, 64, 4096] {
                                let (TransportInspection::Accepted(slow), spent) =
                                    incremental(bytes.clone(), offset, quantum)
                                else {
                                    panic!("expected valid PCR carrier");
                                };
                                assert_eq!(work.spent(), spent);
                                assert_same_packet(&fast, &slow);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn complex_adaptations_and_invalid_pcr_keep_the_incremental_fallback() {
        let pool = Budget::new(8192).unwrap();
        for extra in [1, 2, 4, 8] {
            let mut wire = pcr_carrier(17, 299, 6, extra);
            match extra {
                1 | 2 => wire[12..15].copy_from_slice(&[2, 0x44, 0x55]),
                4 => wire[12] = 0xfe,
                8 => wire.copy_within(6..12, 12),
                _ => unreachable!(),
            }
            let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
            assert_fallback(&bytes, 188);
            assert!(matches!(
                incremental(bytes, 188, 1).0,
                TransportInspection::Accepted(_)
            ));
        }
        for length in 1..7 {
            let mut wire = pcr_carrier(17, 299, 0, 0);
            wire[4] = length;
            let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
            assert_fallback(&bytes, 0);
            let (TransportInspection::Rejected(rejected), _) = incremental(bytes, 0, 1) else {
                panic!("short declared PCR accepted");
            };
            assert_eq!(rejected.error(), Error::Invalid("adaptation field length"));
        }
        for (bad_clock, bad_stuffing) in [(true, false), (false, true), (true, true)] {
            for at in [12, 76, 186] {
                let mut wire = pcr_carrier(17, 299, 175, 0x40);
                if bad_clock {
                    wire[10] ^= 2;
                }
                if bad_stuffing {
                    wire[at] = 0;
                }
                let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
                assert_fallback(&bytes, 188);
                let (TransportInspection::Rejected(rejected), _) = incremental(bytes, 188, 1)
                else {
                    panic!("malformed PCR carrier accepted");
                };
                assert_eq!(
                    rejected.error(),
                    Error::Invalid(if bad_clock {
                        "PCR reserved bits/extension"
                    } else {
                        "adaptation stuffing"
                    })
                );
                assert_eq!(
                    rejected.secondary,
                    (bad_clock && bad_stuffing).then_some(Error::Invalid("adaptation stuffing"))
                );
                assert_eq!(rejected.bytes().as_ref(), wire);
            }
        }
        let mut wire = pcr_carrier(17, 299, 0, 0);
        wire[11] = 0xff; // Extension 511, outside the 27 MHz sub-tick range.
        let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
        assert_fallback(&bytes, 0);
        assert!(matches!(
            incremental(bytes, 0, 1).0,
            TransportInspection::Rejected(_)
        ));

        let wire = pcr_carrier(17, 299, 0, 0);
        for length in 0..188 {
            let bytes = Bytes::copy_from_slice(&pool, &wire[..length]).unwrap();
            assert_fallback(&bytes, 0);
            assert_eq!(
                TransportInspector::detached(bytes, PacketFormat::Ts188, 0, None)
                    .unwrap_err()
                    .error,
                Error::Invalid("transport packet size")
            );
        }
        let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
        for offset in [u64::MAX - 187, u64::MAX] {
            assert_fallback(&bytes, offset);
            assert_eq!(
                TransportInspector::detached(bytes.clone(), PacketFormat::Ts188, offset, None)
                    .unwrap_err()
                    .error,
                Error::Overflow
            );
        }
    }

    #[test]
    fn plain_adaptation_matches_incremental_work_fields_and_payload() {
        let pool = Budget::new(8192).unwrap();
        let mut wire = [0xff; 188];
        wire[..6].copy_from_slice(&[0x47, 0x61, 0, 0x37, 100, 0xe0]);
        wire[105..].fill(0x42);
        let bytes = Bytes::copy_from_slice(&pool, &wire).unwrap();
        let source = Some(SourceId(7));
        let mut fast_work = WorkBudget::new(4096);
        let plain = PlainCarrier::inspect_ts188(&bytes, 188, &mut fast_work).unwrap();
        let fast = plain.into_packet(bytes.clone(), 188, source).unwrap();
        let mut slow_work = WorkBudget::new(4096);
        let mut inspector =
            TransportInspector::detached(bytes, PacketFormat::Ts188, 188, source).unwrap();
        let AssemblyStep::Output(TransportInspection::Accepted(slow)) = inspector
            .poll(&mut slow_work, &AtomicBool::new(false))
            .unwrap()
        else {
            panic!("expected a valid padded carrier");
        };
        assert_eq!(fast_work.spent(), slow_work.spent());
        assert_eq!(fast.pid, slow.pid);
        assert_eq!(fast.payload_source_offset, slow.payload_source_offset);
        assert_eq!(
            fast.payload.unwrap().as_ref(),
            slow.payload.unwrap().as_ref()
        );
        let adaptation = fast.adaptation.unwrap();
        assert!(
            adaptation.discontinuity
                && adaptation.random_access_indicator
                && adaptation.elementary_priority
        );
        assert_eq!(
            adaptation.source.as_ref(),
            slow.adaptation.unwrap().source.as_ref()
        );
        wire[104] = 0;
        let malformed = Bytes::copy_from_slice(&pool, &wire).unwrap();
        let mut work = WorkBudget::new(4096);
        assert!(PlainCarrier::inspect_ts188(&malformed, 0, &mut work).is_none());
        assert_eq!(work.spent(), 0);
    }
}
