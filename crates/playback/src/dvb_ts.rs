//! Explicit DVB service playback from strict TS player outputs.
use crate::{Delivery, Output, RawEvent, TrackKey, TsEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Result, Rounding, SourceId, SourceLocation, TimeBase,
    WorkBudget,
};
use triergon_mpegts::{Pes, TraceEvent, clock::SourceTimestamp, psi::CodecIdentification};
use triergon_subtitles::compat::dvb::{PesPacket, PesParser, PesProgress};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayAction, DisplayEvent, DisplayLimits,
    DisplayProgress, DisplayRegion, PacketEnvelope, PacketIdentity, PacketTiming, ReceiverView,
    Size, SourceClock, SourceConfiguration,
};

/// One selected service from one active PMT. A new playback generation, PMT
/// epoch or TS clock discontinuity needs a new bridge.
pub struct DvbTsBridge {
    pool: Budget,
    source: SourceId,
    track: TrackKey,
    generation: u64,
    configuration_epoch: u64,
    clock_epoch: Option<u64>,
    configuration: SourceConfiguration,
    decoder: Decoder,
    parser: Option<PesParser>,
    rejected_native_pes: Option<PesPacket>,
    packet_limit: usize,
    pending: Option<Pending>,
    sequence: u64,
    accepting: bool,
    eof: bool,
}

struct Pending {
    pes: Pes,
    timing: PacketTiming,
    data: BudgetVec<u8>,
    span: usize,
    offset: usize,
    bytes: Option<Bytes>,
    parser_input: bool,
    native_pes: Option<PesPacket>,
}

/// A service in the active elementary stream's descriptor loop, in wire order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbServiceSelection {
    /// Zero-based ordinal among DVB subtitling descriptors.
    pub descriptor: usize,
    /// Zero-based service entry within that descriptor.
    pub entry: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum DvbQueueAction<'a> {
    Clear { at_ns: i64 },
    Show(DvbQueuePicture<'a>),
}

impl<'a> DvbQueueAction<'a> {
    pub(crate) fn from_native_publication(
        event: &'a DisplayEvent,
        publication: triergon_subtitles::compat::dvb::Publication,
    ) -> Result<Option<Self>> {
        let (clock, timing) = publication
            .packet_clock
            .ok_or(Error::Invalid("DVB queue publication clock absent"))?;
        let start_ns = if let Some(value) = timing.pts.or(timing.dts) {
            let rounding = Rounding::NearestAway;
            let scaled = match i64::try_from(value.ticks) {
                Ok(ticks) => {
                    clock
                        .time_base()
                        .rescale(ticks, TimeBase::new(1, 1_000_000_000)?, rounding)
                }
                Err(_) => value.to_time(clock)?.to_nanoseconds(rounding),
            };
            scaled.unwrap_or(i64::MIN)
        } else {
            0
        };
        if event.action == DisplayAction::Clear {
            return Ok(Some(Self::Clear { at_ns: start_ns }));
        }
        let regions = event.regions();
        let picture = DvbQueuePicture {
            start_ns,
            canvas: publication.context_canvas.filter(|size| {
                (160..=8192).contains(&size.width) && (160..=8192).contains(&size.height)
            }),
            regions: &regions[..regions.len().min(8)],
        };
        Ok(picture.rectangles().next().map(|_| Self::Show(picture)))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DvbQueuePicture<'a> {
    pub start_ns: i64,
    pub canvas: Option<Size>,
    regions: &'a [DisplayRegion],
}

impl<'a> DvbQueuePicture<'a> {
    /// Borrow each indexed rectangle and its native palette in receiver order.
    pub fn rectangles(self) -> impl Iterator<Item = &'a DisplayRegion> {
        self.regions.iter().filter(|region| {
            let plane = &region.plane;
            let width = plane.geometry.stored.width as usize;
            let height = plane.geometry.stored.height as usize;
            (1..=8192).contains(&width)
                && (1..=8192).contains(&height)
                && plane.geometry.stride >= width
                && plane
                    .geometry
                    .stride
                    .checked_mul(height - 1)
                    .and_then(|offset| offset.checked_add(width))
                    .is_some_and(|end| end <= plane.pixels.len())
                && region.palette.receiver.is_some()
        })
    }
}

impl DvbTsBridge {
    /// Require an explicit DVB namespace and a single service descriptor for
    /// this PID. The complete descriptor aliases the active PMT's storage.
    pub fn new(
        output: &Output,
        pid: u16,
        limits: DisplayLimits,
        view: ReceiverView,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::with_service(output, pid, None, limits, view, cancel)
    }

    /// Select one explicitly identified service in the active PMT. The
    /// decoder consumes only that service's composition and ancillary pages.
    pub fn new_for_service(
        output: &Output,
        pid: u16,
        service: DvbServiceSelection,
        limits: DisplayLimits,
        view: ReceiverView,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::with_service(output, pid, Some(service), limits, view, cancel)
    }

    /// Use the stateful native codec packet projection for this selected TS
    /// service. The strict PES decoder constructors keep their existing view.
    pub fn new_with_native_pes_parser(
        output: &Output,
        pid: u16,
        service: Option<DvbServiceSelection>,
        limits: DisplayLimits,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut bridge =
            Self::with_service(output, pid, service, limits, ReceiverView::Receiver, cancel)?;
        bridge.parser = Some(PesParser::new(&bridge.pool));
        Ok(bridge)
    }

    fn with_service(
        output: &Output,
        pid: u16,
        service: Option<DvbServiceSelection>,
        limits: DisplayLimits,
        view: ReceiverView,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let RawEvent::Ts(TsEvent {
            source:
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                },
            ..
        }) = output.event()
        else {
            return Err(Error::Invalid("DVB bridge requires active TS program map"));
        };
        let stream = map
            .streams
            .as_slice()
            .iter()
            .find(|stream| stream.pid == pid)
            .ok_or(Error::Invalid("DVB PID absent from program map"))?;
        let is_dvb = stream.codec_identification() == CodecIdentification::DvbSubtitles;
        let (descriptor, entry) = match service {
            Some(service) if is_dvb => (
                subtitling_descriptor(stream, service.descriptor, cancel)?,
                service.entry,
            ),
            Some(_) => {
                return Err(Error::Unsupported(
                    "DVB bridge requires DVB subtitling signalling",
                ));
            }
            None if is_dvb && stream.descriptor_bytes.len() == 10 => {
                (stream.descriptor_bytes.clone(), 0)
            }
            None => {
                return Err(Error::Unsupported(
                    "DVB bridge requires one subtitling service",
                ));
            }
        };
        let pool = output.pool();
        let configuration = SourceConfiguration::from_subtitling_descriptor(
            pool,
            &descriptor,
            entry,
            SourceClock::new(1, 90_000)?,
            PacketEnvelope::PesDataField,
        )?;
        let generation = output.generation();
        let decoder = Decoder::new_configured_with_receiver_view(
            pool,
            &configuration,
            generation,
            limits,
            view,
            cancel,
        )?;
        Ok(Self {
            pool: pool.clone(),
            source: output.source_id(),
            track: TrackKey::Ts {
                program: map.header.extension,
                pid,
            },
            generation,
            configuration_epoch: *configuration_epoch,
            clock_epoch: None,
            configuration,
            decoder,
            parser: None,
            rejected_native_pes: None,
            packet_limit: limits.packet_bytes,
            pending: None,
            sequence: 0,
            accepting: true,
            eof: false,
        })
    }

    /// Move a selected strict PES only on acceptance. A full mailbox is
    /// retryable and validation errors leave the caller's output intact.
    pub fn push(&mut self, mailbox: &mut Option<Output>) -> Result<bool> {
        let Some(output) = mailbox.as_ref() else {
            return Ok(false);
        };
        if self.eof {
            return Err(Error::Invalid("DVB bridge input after EOF"));
        }
        if !self.accepting {
            return Ok(false);
        }
        if !output.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if output.source_id() != self.source {
            return Err(Error::SourceMismatch);
        }
        if output.generation() != self.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: output.generation(),
            });
        }
        let RawEvent::Ts(TsEvent {
            source: TraceEvent::Pes(pes),
            source_pts,
            source_dts,
            ..
        }) = output.event()
        else {
            return Err(Error::Invalid("DVB bridge requires strict TS PES"));
        };
        if output.delivery() == Delivery::Unselected
            || output.event().track() != Some(self.track)
            || pes.header.stream_id != 0xbd
            || pes.epochs.configuration != self.configuration_epoch
        {
            return Err(Error::Invalid("DVB bridge track or configuration changed"));
        }
        if self
            .clock_epoch
            .is_some_and(|clock| clock != pes.epochs.clock)
        {
            return Err(Error::Invalid("DVB bridge clock epoch changed"));
        }
        if pes.payload_len == 0 || pes.spans().is_empty() {
            return Err(Error::Invalid("DVB bridge empty PES"));
        }
        if pes.payload_len > self.packet_limit {
            return Err(Error::Limit {
                requested: pes.payload_len,
                maximum: self.packet_limit,
            });
        }
        let timing = PacketTiming {
            pts: clock(*source_pts),
            dts: clock(*source_dts),
            duration: None,
        };
        let sequence = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        let RawEvent::Ts(TsEvent {
            source: TraceEvent::Pes(pes),
            ..
        }) = mailbox.take().expect("validated PES output").into_event()
        else {
            unreachable!("validated PES event")
        };
        self.clock_epoch = Some(pes.epochs.clock);
        self.sequence = sequence;
        self.pending = Some(Pending {
            pes,
            timing,
            data: BudgetVec::new(&self.pool),
            span: 0,
            offset: 0,
            bytes: None,
            parser_input: false,
            native_pes: None,
        });
        self.accepting = false;
        Ok(true)
    }

    /// Recover the original PES and projected bytes from a native packet
    /// rejected before decoder admission. The next rejection replaces it.
    pub fn take_rejected_native_pes(&mut self) -> Option<PesPacket> {
        self.rejected_native_pes.take()
    }

    pub fn finish_input(&mut self) {
        self.eof = true;
    }

    /// Project a native publication onto the packet-clock bitmap queue.
    /// Its clear, rectangle order and authoring canvas are independent of
    /// the source display set's timing and expiry.
    pub fn packet_clock_action<'a>(
        &self,
        event: &'a DisplayEvent,
    ) -> Result<Option<DvbQueueAction<'a>>> {
        if event.generation != self.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: event.generation,
            });
        }
        if event.service != self.configuration.service() {
            return Err(Error::Invalid("DVB queue event service"));
        }
        let source = event
            .source_packets()
            .first()
            .ok_or(Error::Invalid("DVB queue event source absent"))?;
        if !source.payload.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        let facts = source.source.as_ref().ok_or(Error::SourceMismatch)?;
        if facts
            .container_position()
            .is_none_or(|position| position.source != self.source)
        {
            return Err(Error::SourceMismatch);
        }
        let config = facts.configuration();
        if config.source() != self.configuration.source()
            || !std::ptr::eq(
                config.original().as_ref(),
                self.configuration.original().as_ref(),
            )
        {
            return Err(Error::Invalid("DVB queue event configuration"));
        }
        let publication = event
            .receiver_publication
            .ok_or(Error::Unsupported("DVB queue requires native publication"))?;
        DvbQueueAction::from_native_publication(event, publication)
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DisplayProgress> {
        if cancel.load(Ordering::Relaxed) {
            self.pending = None;
            self.rejected_native_pes = None;
            if let Some(parser) = &mut self.parser {
                parser.reset();
            }
            self.eof = true;
            return self.decoder.step(work, cancel);
        }
        if let Some(pending) = &mut self.pending {
            match pending.collect(&self.pool, work) {
                Ok(true) => {}
                Ok(false) => return Ok(progress(DecodeStatus::Yield, None)),
                Err(error @ Error::Backpressure { .. }) => {
                    return Ok(progress(DecodeStatus::NeedOutput, Some(error)));
                }
                Err(error) => return Err(error),
            }
            if let Some(parser) = &mut self.parser {
                match pending.project(parser, work, cancel) {
                    Ok(true) => {}
                    Ok(false) => return Ok(progress(DecodeStatus::Yield, None)),
                    Err(error @ Error::Backpressure { .. }) => {
                        return Ok(progress(DecodeStatus::NeedOutput, Some(error)));
                    }
                    Err(error) => return Err(error),
                }
            }
            let position = pending
                .pes
                .header_source_ranges
                .as_slice()
                .first()
                .map(|range| SourceLocation {
                    source: range.entity,
                    offset: range.offset,
                });
            let packet = self.configuration.packet(
                &self.pool,
                pending
                    .bytes
                    .as_ref()
                    .ok_or(Error::Invalid("DVB PES data absent"))?,
                pending.timing,
                PacketIdentity {
                    generation: self.generation,
                    sequence: self.sequence,
                    container_position: position,
                },
            )?;
            if let Some(parsed) = pending.native_pes.take() {
                if let Err(error) = self.decoder.provide_pes(&packet, &parsed) {
                    self.rejected_native_pes = Some(parsed);
                    self.pending = None;
                    self.accepting = true;
                    return Err(error);
                }
            } else {
                self.decoder.provide(&packet)?;
            }
            self.pending = None;
        }
        if self.eof {
            self.parser = None;
            self.decoder.finish_input()?;
        }
        let progress = self.decoder.step(work, cancel)?;
        self.accepting = progress.status == DecodeStatus::NeedPacket;
        Ok(progress)
    }
}

fn subtitling_descriptor(
    stream: &triergon_mpegts::psi::ElementaryStream,
    selected: usize,
    cancel: &AtomicBool,
) -> Result<Bytes> {
    let mut index = 0;
    let mut offset = 0_usize;
    for descriptor in stream.descriptors() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let descriptor = descriptor?;
        let end = offset
            .checked_add(descriptor.source.len())
            .ok_or(Error::Overflow)?;
        if descriptor.tag == 0x59 {
            if index == selected {
                return stream.descriptor_bytes.slice(offset..end);
            }
            index += 1;
        }
        offset = end;
    }
    Err(Error::Invalid("DVB subtitling descriptor index"))
}

impl Pending {
    fn project(
        &mut self,
        parser: &mut PesParser,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.native_pes.is_some() {
            return Ok(true);
        }
        if !self.parser_input {
            let raw = self
                .bytes
                .as_ref()
                .ok_or(Error::Invalid("DVB PES data absent"))?;
            parser.provide(raw, self.timing.pts.map(|pts| pts.ticks))?;
            self.parser_input = true;
        }
        match parser.step(work, cancel)? {
            PesProgress::Yield => Ok(false),
            PesProgress::Packet(packet) => {
                self.native_pes = Some(packet);
                Ok(true)
            }
            PesProgress::NeedPacket => Err(Error::Invalid("DVB parser input disappeared")),
        }
    }

    fn collect(&mut self, pool: &Budget, work: &mut WorkBudget) -> Result<bool> {
        if self.bytes.is_some() {
            return Ok(true);
        }
        let spans = self.pes.spans();
        if let [span] = spans {
            if !work.try_charge(1) {
                return Ok(false);
            }
            self.bytes = Some(span.payload_bytes()?);
            return Ok(true);
        }
        if self.data.capacity() == 0 {
            if !work.try_charge(1) {
                return Ok(false);
            }
            self.data.try_reserve_exact(self.pes.payload_len)?;
        }
        while let Some(span) = spans.get(self.span) {
            let remaining = &span.as_ref()[self.offset..];
            let take = work.take(remaining.len() as u64) as usize;
            if take == 0 && !remaining.is_empty() {
                return Ok(false);
            }
            self.data.try_extend_from_slice(&remaining[..take])?;
            if take == remaining.len() {
                self.span += 1;
                self.offset = 0;
            } else {
                self.offset += take;
            }
        }
        if self.data.len() != self.pes.payload_len {
            return Err(Error::Invalid("DVB PES span length"));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match std::mem::replace(&mut self.data, BudgetVec::new(pool)).freeze() {
            Ok(bytes) => {
                self.bytes = Some(bytes);
                Ok(true)
            }
            Err(error) => {
                self.data = error.buffer;
                Err(error.error)
            }
        }
    }
}

fn clock(timestamp: Option<SourceTimestamp>) -> Option<ClockValue> {
    let timestamp = timestamp?;
    let ticks = timestamp.ticks90?;
    Some(ClockValue {
        ticks,
        origin: if timestamp.raw33.map(i128::from) == Some(ticks) {
            ClockOrigin::Source
        } else {
            ClockOrigin::Inferred
        },
    })
}

fn progress(status: DecodeStatus, pressure: Option<Error>) -> DisplayProgress {
    DisplayProgress {
        status,
        event: None,
        pressure,
    }
}
