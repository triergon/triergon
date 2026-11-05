//! Owned strict transport inspection. Work advances actual bounded fields/bytes.
use crate::{Adaptation, AssemblyStep, PacketFormat, Pcr, SourceRange, TransportPacket};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, PushError, Result, SourceId, SourceLocation, WorkBudget,
};

pub(crate) struct CarrierHeader {
    pid: u16,
    pusi: bool,
    priority: bool,
    tei: bool,
    scrambling: u8,
    cc: u8,
    pub(crate) afc: u8,
}

impl CarrierHeader {
    pub(crate) fn parse(data: &[u8]) -> Result<Self> {
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

    pub(crate) fn payload_only_ts188(
        self,
        source: Bytes,
        source_offset: u64,
        source_id: Option<SourceId>,
    ) -> Result<TransportPacket> {
        let payload = source.slice(4..188)?;
        Ok(TransportPacket {
            source,
            source_offset,
            source_id,
            prefix: None,
            pid: self.pid,
            payload_unit_start: self.pusi,
            transport_priority: self.priority,
            transport_error: self.tei,
            scrambling: self.scrambling,
            continuity_counter: self.cc,
            adaptation: None,
            payload: Some(payload),
            payload_source_offset: source_offset.checked_add(4).ok_or(Error::Overflow)?,
            sync: 0,
            pcr_range: None,
        })
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
    fn end(&self, count: usize) -> Result<usize> {
        let next = self.position.checked_add(count).ok_or(Error::Overflow)?;
        if next > self.fields.adaptation_end {
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
                if let Some(invalid) = stuffing.iter().position(|&byte| byte != 0xff) {
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
                .ok_or(Error::Invalid("carrier owner missing"))?
                .clone();
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
                        self.phase = Phase::Pcr;
                    }
                    Phase::Pcr => {
                        if self.fields.flags & 0x10 != 0 {
                            let next = self.end(6)?;
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
                        self.phase = Phase::Opcr;
                    }
                    Phase::Opcr => {
                        if self.fields.flags & 8 != 0 {
                            let next = self.end(6)?;
                            self.fields.opcr =
                                Some(crate::transport::pcr(&data[self.position..next])?);
                            self.position = next;
                        }
                        self.phase = Phase::Splice;
                    }
                    Phase::Splice => {
                        if self.fields.flags & 4 != 0 {
                            let next = self.end(1)?;
                            self.fields.splice = Some(data[self.position] as i8);
                            self.position = next;
                        }
                        self.phase = Phase::Private;
                    }
                    Phase::Private | Phase::Extension => {
                        let private = matches!(self.phase, Phase::Private);
                        let flag = if private { 2 } else { 1 };
                        if self.fields.flags & flag != 0 {
                            self.end(1)?;
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
