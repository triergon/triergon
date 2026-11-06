//! Complete-packet method-0 DVB service decoder. Format boundaries are explicit;
//! an input entity End never synthesizes EOD, a clear cue or a codec reset.
use super::bits::cancelled;
use super::boundary::{ClockOrigin, ClockValue, PacketTiming, SourceClock, SourceConfiguration};
use super::display_model::*;
use super::display_render::Renderer;
use super::display_state::{DecodeWork, Segment, SegmentResult, State, limit};
use super::pes_parser::NativePesPacket;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, Time, WorkBudget};

#[derive(Debug)]
struct PendingPacket {
    packet: DvbPacket,
    native_pes: Option<NativePesPacket>,
    effective_pts: Option<Time>,
    offset: usize,
    end: usize,
    selected_display_ended: bool,
    canvas_segments: u8,
}

impl PendingPacket {
    fn bytes(&self) -> &Bytes {
        self.native_pes
            .as_ref()
            .map_or(&self.packet.payload, NativePesPacket::codec)
    }

    fn packet_clock(&self) -> Option<(SourceClock, PacketTiming)> {
        let facts = self.packet.source.as_ref()?;
        let mut timing = facts.timing();
        if let Some(parsed) = &self.native_pes
            && parsed.codec_pts() != parsed.source_pts()
        {
            timing.pts = parsed.codec_pts().map(|ticks| ClockValue {
                ticks,
                origin: ClockOrigin::Inferred,
            });
        }
        Some((facts.configuration().clock(), timing))
    }
}

const CANVAS_SEGMENTS: u8 = 0b111;

#[derive(Debug)]
pub struct Decoder {
    state: State,
    configuration: Option<SourceConfiguration>,
    generation: u64,
    sequence: u64,
    last_input_sequence: Option<u64>,
    last_pts: Option<Time>,
    packet: Option<PendingPacket>,
    segment: Option<Segment>,
    renderer: Option<Renderer>,
    packets: BudgetVec<DvbPacket>,
    display_started: bool,
    pts: Option<Time>,
    segments: usize,
    header_credit: usize,
    total_work: usize,
    input_finished: bool,
    failed: Option<Error>,
    native_failure: Option<super::NativeFailure>,
    native_fallback: bool,
}

impl Decoder {
    pub fn new(
        budget: &Budget,
        service: Service,
        generation: u64,
        limits: DisplayLimits,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::new_with_object_retention(
            budget,
            service,
            generation,
            limits,
            ObjectRetention::CurrentDisplay,
            cancel,
        )
    }

    pub fn new_with_object_retention(
        budget: &Budget,
        service: Service,
        generation: u64,
        limits: DisplayLimits,
        object_retention: ObjectRetention,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        cancelled(cancel)?;
        if limits.packet_bytes == 0 || limits.regions == 0 || limits.work_per_display == 0 {
            return Err(Error::Invalid("DVB zero decoder policy"));
        }
        Ok(Self {
            state: State::new(budget, service, limits, object_retention),
            configuration: None,
            generation,
            sequence: 0,
            last_input_sequence: None,
            last_pts: None,
            packet: None,
            segment: None,
            renderer: None,
            packets: BudgetVec::new(budget),
            display_started: false,
            pts: None,
            segments: 0,
            header_credit: 0,
            total_work: 0,
            input_finished: false,
            failed: None,
            native_failure: None,
            native_fallback: false,
        })
    }

    /// Bind an explicit post-demux input declaration. CurrentDisplay remains an
    /// implementation policy, with no mandatory cached-compressed-ODS claim.
    pub fn new_configured(
        budget: &Budget,
        configuration: &SourceConfiguration,
        generation: u64,
        limits: DisplayLimits,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::new_configured_with_object_retention(
            budget,
            configuration,
            generation,
            limits,
            ObjectRetention::CurrentDisplay,
            cancel,
        )
    }

    pub fn new_configured_with_object_retention(
        budget: &Budget,
        configuration: &SourceConfiguration,
        generation: u64,
        limits: DisplayLimits,
        object_retention: ObjectRetention,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        cancelled(cancel)?;
        configuration.check_pool(budget)?;
        let mut decoder = Self::new_with_object_retention(
            budget,
            configuration.service(),
            generation,
            limits,
            object_retention,
            cancel,
        )?;
        decoder.configuration = Some(configuration.clone());
        Ok(decoder)
    }

    /// The named native view is additive; the old constructor stays source-only.
    pub fn new_configured_with_receiver_view(
        budget: &Budget,
        configuration: &SourceConfiguration,
        generation: u64,
        limits: DisplayLimits,
        view: super::ReceiverView,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut decoder = Self::new_configured(budget, configuration, generation, limits, cancel)?;
        decoder.state.receiver_view = view;
        Ok(decoder)
    }
    /// Additive native projection. Both older constructors retain strict
    /// bounds validation, including empty regions. No source packet is trimmed.
    pub fn new_configured_with_native_geometry_policy(
        budget: &Budget,
        configuration: &SourceConfiguration,
        generation: u64,
        limits: DisplayLimits,
        policy: super::NativeGeometryPolicy,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut decoder = Self::new_configured_with_receiver_view(
            budget,
            configuration,
            generation,
            limits,
            super::ReceiverView::NativeD32,
            cancel,
        )?;
        decoder.state.native_geometry_policy = policy;
        Ok(decoder)
    }

    pub fn native_failure(&self) -> Option<super::NativeFailure> {
        self.native_failure
    }

    /// Caller keeps its immutable packet on rejection. Whole codec packets are
    /// supplied after core InputCursor/demux validation, not a second I/O window.
    pub fn provide(&mut self, packet: &DvbPacket) -> Result<()> {
        self.admit(packet, None)
    }

    /// Decode the explicit native codec packet while retaining the original PES
    /// and its source declaration. A pass-through keeps the same decoder syntax
    /// validation as any other codec packet.
    pub fn provide_native_pes(
        &mut self,
        packet: &DvbPacket,
        parsed: &NativePesPacket,
    ) -> Result<()> {
        self.admit(packet, Some(parsed))
    }

    fn admit(&mut self, packet: &DvbPacket, parsed: Option<&NativePesPacket>) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if packet.generation != self.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: packet.generation,
            });
        }
        if !packet.payload.belongs_to(&self.state.budget) {
            return Err(Error::ForeignPool);
        }
        if self.packet.is_some()
            || self.segment.is_some()
            || self.renderer.is_some()
            || self.input_finished
        {
            return Err(Error::Invalid("DVB packet not currently requested"));
        }
        if self
            .last_input_sequence
            .is_some_and(|last| packet.sequence <= last)
        {
            return Err(Error::Invalid("DVB input packet order"));
        }
        if let (Some(previous), Some(current)) = (self.last_pts, packet.pts)
            && current < previous
        {
            return Err(Error::Invalid(
                "DVB input PTS order requires discontinuity reset",
            ));
        }
        if let Some(source) = &packet.source {
            source.validate(&self.state.budget, packet, self.state.service)?;
            if self.configuration.as_ref().is_some_and(|configuration| {
                !configuration.same_decode_boundary(source.configuration())
            }) {
                return Err(Error::Invalid(
                    "DVB configured source changed without reset",
                ));
            }
        } else if self.configuration.is_some() {
            return Err(Error::Invalid("DVB configured packet source facts absent"));
        }
        limit(packet.payload.len(), self.state.limits.packet_bytes)?;
        let (offset, end, effective_pts) = if let Some(parsed) = parsed {
            if self.state.receiver_view != super::ReceiverView::NativeD32
                || packet.envelope != PacketEnvelope::PesDataField
            {
                return Err(Error::Unsupported("DVB native PES decoder boundary"));
            }
            let facts = packet.source.as_ref().ok_or(Error::SourceMismatch)?;
            if !std::ptr::eq(packet.payload.as_ref(), parsed.pes().as_ref())
                || facts.timing().pts.map(|value| value.ticks) != parsed.source_pts()
            {
                return Err(Error::SourceMismatch);
            }
            if !parsed.codec().belongs_to(&self.state.budget) {
                return Err(Error::ForeignPool);
            }
            limit(parsed.codec().len(), self.state.limits.packet_bytes)?;
            if parsed.codec().len() <= 6 || parsed.codec().first() != Some(&0x0f) {
                return Err(Error::Invalid("DVB native codec packet header"));
            }
            let pts = parsed
                .codec_pts()
                .map(|ticks| Time::from_ticks(ticks, facts.configuration().clock().time_base()))
                .transpose()?;
            (0, parsed.codec().len(), pts)
        } else {
            let (offset, end) = match packet.envelope {
                PacketEnvelope::Segments => {
                    if packet.payload.is_empty() {
                        return Err(Error::Invalid("DVB empty segment packet"));
                    }
                    (0, packet.payload.len())
                }
                PacketEnvelope::PesDataField => {
                    let bytes = packet.payload.as_ref();
                    if bytes.len() < 3 {
                        return Err(Error::Truncated);
                    }
                    if bytes[..2] != [0x20, 0] || bytes.last() != Some(&0xff) {
                        return Err(Error::Invalid("DVB PES data-field envelope"));
                    }
                    if bytes.len() == 3 {
                        return Err(Error::Invalid("DVB empty PES data field"));
                    }
                    (2, bytes.len() - 1)
                }
            };
            (offset, end, packet.pts)
        };
        self.packet = Some(PendingPacket {
            packet: packet.clone(),
            native_pes: parsed.cloned(),
            effective_pts,
            offset,
            end,
            selected_display_ended: false,
            canvas_segments: 0,
        });
        if self.configuration.is_none() {
            self.configuration = packet
                .source
                .as_ref()
                .map(|source| source.configuration().clone());
        }
        self.last_input_sequence = Some(packet.sequence);
        self.last_pts = packet.pts.or(self.last_pts);
        Ok(())
    }

    pub fn finish_input(&mut self) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.input_finished = true;
        Ok(())
    }

    /// Logical generation changes require an explicit reset/acquisition. Range
    /// requests and successive source entities must not call this automatically.
    /// Keep the caller-selected receiver view while discarding stream state.
    pub fn reset(&mut self, generation: u64, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        if generation == self.generation {
            return Err(Error::Invalid("DVB reset requires a new generation"));
        }
        let mut state = State::new(
            &self.state.budget,
            self.state.service,
            self.state.limits,
            self.state.object_retention,
        );
        state.receiver_view = self.state.receiver_view;
        state.native_geometry_policy = self.state.native_geometry_policy;
        self.state = state;
        self.generation = generation;
        self.sequence = 0;
        self.last_input_sequence = None;
        self.last_pts = None;
        self.packet = None;
        self.segment = None;
        self.renderer = None;
        self.packets = BudgetVec::new(&self.state.budget);
        self.display_started = false;
        self.pts = None;
        self.segments = 0;
        self.header_credit = 0;
        self.total_work = 0;
        self.input_finished = false;
        self.failed = None;
        self.native_failure = None;
        self.native_fallback = false;
        Ok(())
    }

    /// A changed decoding declaration requires an explicit generation reset.
    /// Retained output aliases keep their previous configuration and raw clock.
    /// Receiver view, limits and object retention remain caller-selected policy.
    pub fn reset_with_configuration(
        &mut self,
        configuration: &SourceConfiguration,
        generation: u64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        cancelled(cancel)?;
        if generation == self.generation {
            return Err(Error::Invalid("DVB reset requires a new generation"));
        }
        let mut next = Self::new_configured_with_object_retention(
            &self.state.budget,
            configuration,
            generation,
            self.state.limits,
            self.state.object_retention,
            cancel,
        )?;
        next.state.receiver_view = self.state.receiver_view;
        next.state.native_geometry_policy = self.state.native_geometry_policy;
        *self = next;
        Ok(())
    }

    fn poison(&mut self, error: Error) {
        self.failed = Some(error);
        self.packet = None;
        self.segment = None;
        self.renderer = None;
        self.packets = BudgetVec::new(&self.state.budget);
        let view = self.state.receiver_view;
        let geometry = self.state.native_geometry_policy;
        self.state = State::new(
            &self.state.budget,
            self.state.service,
            self.state.limits,
            self.state.object_retention,
        );
        self.state.receiver_view = view;
        self.state.native_geometry_policy = geometry;
    }

    fn progress(status: DecodeStatus) -> DisplayProgress {
        DisplayProgress {
            status,
            event: None,
            pressure: None,
        }
    }

    fn include_packet(&mut self, packet: &DvbPacket, pts: Option<Time>) -> Result<()> {
        if self.display_started && pts != self.pts {
            return Err(Error::Invalid("DVB display-set PTS changed"));
        }
        if self
            .packets
            .as_slice()
            .last()
            .is_none_or(|p| p.sequence != packet.sequence)
        {
            limit(
                self.packets.len() + 1,
                self.state.limits.packets_per_display,
            )?;
            self.packets.try_push(packet.clone()).map_err(|e| e.error)?;
        }
        self.pts = pts;
        self.display_started = true;
        Ok(())
    }

    fn header(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("DVB packet absent"))?;
        let remaining = packet
            .end
            .checked_sub(packet.offset)
            .ok_or(Error::Overflow)?;
        if remaining < 6 {
            if self.state.receiver_view == super::ReceiverView::NativeD32 {
                if self.display_started && self.state.acquired {
                    // Actual106/101 codec inputs establish publication of the
                    // complete source prefix before the original tail error.
                    // The packet and five tail bytes remain owned and intact.
                    self.native_fallback = true;
                    self.renderer = Some(Renderer::new(&self.state.budget));
                    return Ok(true);
                }
                self.native_failure = Some(super::NativeFailure {
                    packet_sequence: packet.packet.sequence,
                    packet_offset: packet.offset,
                    unconsumed_bytes: remaining,
                    code: -1094995529, // observed/public AVERROR_INVALIDDATA
                });
            }
            return Err(Error::Truncated);
        }
        let mut charged = DecodeWork {
            work,
            total: &mut self.total_work,
            limit: self.state.limits.work_per_display,
            cancel,
        };
        if !charged.pay(&mut self.header_credit, 6)? {
            return Ok(false);
        }
        let bytes = packet.bytes().as_ref();
        let at = packet.offset;
        if bytes[at] != 15 {
            return Err(Error::Invalid("DVB segment sync"));
        }
        let kind = bytes[at + 1];
        let page = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]);
        let length = usize::from(u16::from_be_bytes([bytes[at + 4], bytes[at + 5]]));
        let end = at
            .checked_add(6)
            .and_then(|v| v.checked_add(length))
            .ok_or(Error::Overflow)?;
        if end > packet.end {
            return Err(Error::Truncated);
        }
        if packet.packet.envelope == PacketEnvelope::PesDataField
            && packet.native_pes.is_none()
            && packet.selected_display_ended
            && page == self.state.service.composition_page
        {
            return Err(Error::Invalid(
                "DVB multiple display sets in PES data field",
            ));
        }
        limit(
            self.segments.checked_add(1).ok_or(Error::Overflow)?,
            self.state.limits.segments_per_display,
        )?;
        let source = packet.packet.clone();
        let original = packet.bytes().slice(at..end)?;
        let pts = packet.effective_pts;
        let selected = self.state.page_slot(page).is_some();
        if selected {
            self.include_packet(&source, pts)?;
        }
        let segment = Segment::new(kind, page, original)?;
        let packet = self
            .packet
            .as_mut()
            .ok_or(Error::Invalid("DVB packet absent"))?;
        packet.offset = end;
        if selected && self.state.receiver_view == super::ReceiverView::NativeD32 {
            packet.canvas_segments |= match kind {
                0x10 => 0b001,
                0x11 => 0b010,
                0x13 => 0b100,
                _ => 0,
            };
        }
        self.segment = Some(segment);
        self.segments += 1;
        self.header_credit = 0;
        Ok(true)
    }

    fn build_event(&mut self, renderer: Renderer) -> Result<DisplayEvent> {
        self.establish_default_native_canvas();
        let expiry = self
            .pts
            .map(|time| time.checked_add(Time::new(i128::from(self.state.timeout), 1)?))
            .transpose()?;
        let has_visible = match self.state.receiver_view {
            super::ReceiverView::Source => !self.state.visible.is_empty(),
            super::ReceiverView::NativeD32 => !renderer.output.is_empty(),
        };
        let action = if !has_visible {
            DisplayAction::Clear
        } else if self.state.had_visible {
            DisplayAction::Update
        } else {
            DisplayAction::Present
        };
        self.state.had_visible = has_visible;
        let packets = std::mem::replace(&mut self.packets, BudgetVec::new(&self.state.budget));
        let opaque = std::mem::replace(&mut self.state.opaque, BudgetVec::new(&self.state.budget));
        let native_publication = if self.state.receiver_view == super::ReceiverView::NativeD32 {
            self.packet.as_ref().map(|packet| super::NativePublication {
                identifier: super::D32_VIEW,
                source_pin: super::D32_PIN,
                packet_sequence: packet.packet.sequence,
                consumed_prefix_bytes: packet.offset,
                reason: if self.native_fallback {
                    super::PublicationReason::CompletePrefix
                } else {
                    super::PublicationReason::EndOfDisplay
                },
                packet_clock: packet.packet_clock(),
                context_canvas: self.state.native_canvas,
                exact_pts_microseconds: super::native_view::exact_microseconds(self.pts),
                start_display_ms: 0,
                end_display_ms: u32::from(self.state.timeout) * 1000,
            })
        } else {
            None
        };
        let event = DisplayEvent {
            receiver_view: self.state.receiver_view,
            native_publication,
            native_pes: self
                .packet
                .as_ref()
                .and_then(|packet| packet.native_pes.clone()),
            native_geometry_policy: self.state.native_geometry_policy,
            omitted_regions: renderer.omitted,
            service: self.state.service,
            object_retention: self.state.object_retention,
            generation: self.generation,
            epoch: self.state.epoch,
            sequence: self.sequence,
            pts: self.pts,
            timeout_seconds: self.state.timeout,
            expiry,
            action,
            definition: self.state.definition.clone(),
            page_composition: self.state.page_composition.clone(),
            page_version: self.state.page_version,
            page_state: self.state.page_state,
            regions: renderer.output,
            packets,
            opaque,
        };
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        self.state.finish_display();
        self.display_started = false;
        self.native_fallback = false;
        self.pts = None;
        self.segments = 0;
        self.total_work = 0;
        Ok(event)
    }

    fn establish_default_native_canvas(&mut self) {
        if self
            .packet
            .as_ref()
            .is_some_and(|p| p.canvas_segments == CANVAS_SEGMENTS)
        {
            self.state
                .native_canvas
                .get_or_insert_with(|| DisplayDefinition::default().size);
        }
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DisplayProgress> {
        loop {
            cancelled(cancel)?;
            if let Some(renderer) = self.renderer.as_mut() {
                let mut charged = DecodeWork {
                    work,
                    total: &mut self.total_work,
                    limit: self.state.limits.work_per_display,
                    cancel,
                };
                if !renderer.step(&mut self.state, &mut charged)? {
                    return Ok(Self::progress(DecodeStatus::Yield));
                }
                let renderer = self
                    .renderer
                    .take()
                    .ok_or(Error::Invalid("DVB renderer absent"))?;
                let event = self.build_event(renderer)?;
                return Ok(DisplayProgress {
                    status: DecodeStatus::Event,
                    event: Some(event),
                    pressure: None,
                });
            }
            if let Some(segment) = self.segment.as_mut() {
                let mut charged = DecodeWork {
                    work,
                    total: &mut self.total_work,
                    limit: self.state.limits.work_per_display,
                    cancel,
                };
                match segment.step(&mut self.state, &mut charged)? {
                    SegmentResult::Yield => return Ok(Self::progress(DecodeStatus::Yield)),
                    SegmentResult::Complete => self.segment = None,
                    SegmentResult::EndDisplay => {
                        if !self.state.acquired {
                            return Err(Error::Unsupported("DVB acquisition required"));
                        }
                        self.packet
                            .as_mut()
                            .ok_or(Error::Invalid("DVB packet absent at EOD"))?
                            .selected_display_ended = true;
                        self.segment = None;
                        self.renderer = Some(Renderer::new(&self.state.budget));
                    }
                }
                continue;
            }
            if self.packet.as_ref().is_some_and(|p| p.offset == p.end) {
                self.establish_default_native_canvas();
                if self.state.receiver_view == super::ReceiverView::NativeD32
                    && self.display_started
                    && self.state.acquired
                {
                    self.native_fallback = true;
                    self.renderer = Some(Renderer::new(&self.state.budget));
                    continue;
                }
                self.packet = None;
            }
            if self.packet.is_none() {
                if self.input_finished {
                    if self.display_started {
                        return Err(Error::Truncated);
                    }
                    return Ok(Self::progress(DecodeStatus::Finished));
                }
                return Ok(Self::progress(DecodeStatus::NeedPacket));
            }
            if !self.header(work, cancel)? {
                return Ok(Self::progress(DecodeStatus::Yield));
            }
        }
    }

    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DisplayProgress> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        match self.step_inner(work, cancel) {
            Ok(progress) => Ok(progress),
            Err(error @ Error::Backpressure { .. }) => Ok(DisplayProgress {
                status: DecodeStatus::NeedOutput,
                event: None,
                pressure: Some(error),
            }),
            Err(error) => {
                self.poison(error);
                Err(error)
            }
        }
    }
}
