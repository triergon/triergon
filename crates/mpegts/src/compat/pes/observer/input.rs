//! Admission of transport carriers: pushes, bounds, input binding and recovery hooks.
#[cfg(feature = "reference-trace")]
use super::DemuxScope;
use super::{CarrierLayout, DemuxObserver, DemuxSpan, Pending, SourceContext};
use crate::SourceRange;
use crate::transport::TransportPacket;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Refused, Result, WorkBudget};

impl DemuxObserver {
    pub(crate) fn validate_input(&self, input: Option<&triergon_core::InputCursor>) -> Result<()> {
        match (&self.demux_input, input) {
            (Some(context), Some(input)) => context.validate_cursor(input),
            (Some(_), None) => Err(Error::Invalid(
                "bound native operation requires input cursor",
            )),
            (None, _) => Ok(()),
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "recover the original charged inline carrier"
    )]
    pub fn push_with_input(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        input: &triergon_core::InputCursor,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        self.push_input_boundary(packet, context, false, input)
    }

    #[allow(
        clippy::result_large_err,
        reason = "recover the original charged inline carrier"
    )]
    pub(crate) fn push_input_boundary(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        boundary: bool,
        input: &triergon_core::InputCursor,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        self.push_boundary(packet, context, boundary, Some(input))
    }

    /// Retain the same logical TS/PES/demux-clock operation across a confirmed
    /// entity End. Only physical entity/order and input-binding facts change.
    /// A false result is a work yield; every rejection leaves assembly intact.
    pub fn continue_media_entity(
        &mut self,
        end: &crate::stream::MediaEntityEnd,
        input: &mut triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(false);
        }
        end.validate_continuation(input)?;
        if !self.budget.same_pool(&end.budget) {
            return Err(Error::ForeignPool);
        }
        if end.location.source != self.scope.entity
            || end.epochs != self.scope.epochs
            || end.program != self.scope.program
        {
            return Err(Error::Invalid("native media entity source scope mismatch"));
        }
        if self.pending.is_some()
            || self.emission.is_some()
            || self.growth.is_some()
            || self.fault.is_some()
            || self.ended
        {
            return Err(Error::Invalid("native media entity operation not drained"));
        }
        let position = self
            .media_position_base
            .checked_add(end.location.offset)
            .ok_or(Error::Overflow)?;
        i64::try_from(position).map_err(|_| Error::Overflow)?;
        let binding = self
            .demux_input
            .as_ref()
            .map(|b| b.continued_media_entity(end, input))
            .transpose()?;
        // An empty core read checks sticky transport/cancellation failure without
        // consuming an already supplied new-entity window.
        input.read_into(&mut [], work, cancel)?;
        if !work.try_charge(1) {
            return Ok(false);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.scope.entity = input.source();
        self.demux_input = binding;
        self.entity_first = true;
        self.media_position_base = position;
        Ok(true)
    }

    pub(crate) fn media_position(&self, offset: u64) -> Result<i64> {
        i64::try_from(
            self.media_position_base
                .checked_add(offset)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)
    }

    /// Only the selected PID, in source order. Rejected input is recovered
    /// intact without a fresh allocation. Drain poll before another push.
    #[allow(
        clippy::result_large_err,
        reason = "recover original inline charged transport input; no uncharged Box allocation"
    )]
    pub fn push(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        self.push_boundary(packet, context, false, None)
    }

    /// Admit a carrier while the caller keeps the original transport event.
    /// The observer retains only the storage and fields used to assemble PES.
    pub fn push_borrowed(
        &mut self,
        packet: &TransportPacket,
        context: SourceContext,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<()> {
        if let Some(error) = self.input_error(packet, context, false, input, false) {
            return Err(error);
        }
        self.retain_carrier(
            CarrierLayout::from(packet),
            packet.source.clone(),
            packet.payload.clone(),
            context,
        );
        Ok(())
    }

    #[allow(
        clippy::result_large_err,
        reason = "return original charged transport input inline"
    )]
    pub(crate) fn push_clock_boundary(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        self.push_boundary(packet, context, true, None)
    }

    pub(crate) fn commit_clock_epoch(&mut self, context: SourceContext) {
        self.scope.epochs = context.epochs;
        // The Timeline consumes an ordinal, not another physical carrier.
        if let Some((ordinal, _)) = self.last_input.as_mut() {
            *ordinal = (*ordinal).max(context.source_event_ordinal);
        }
    }

    /// A drained codec-map commit. Returned old packets keep their original
    /// headers/storage. Retire only the idle internal header alias; preserve wire
    /// counter/order and charged arena capacity, without copying a payload.
    #[cfg(feature = "reference-trace")]
    pub(crate) fn commit_pmt_scope(&mut self, scope: DemuxScope) {
        self.scope = scope;
        self.header = None;
        self.origin = None;
    }

    pub(crate) fn recovery_counter(&self) -> Option<u8> {
        self.previous_counter
    }

    pub(crate) fn transport_fault_input(
        &self,
        packet: &TransportPacket,
        context: SourceContext,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<&Budget> {
        if let Some(error) = self.input_error(packet, context, false, input, true) {
            return Err(error);
        }
        if !self.recovery_idle() {
            return Err(Error::Invalid("selected diagnostic input not drained"));
        }
        if self.scope.stream_type != 0x1b
            || self.scope.stream_id != 0xe0
            || context.byte_run_epoch.is_some()
        {
            return Err(Error::Unsupported(
                "selected diagnostic source context unobserved",
            ));
        }
        Ok(&self.budget)
    }

    /// Idle assembly retains arena capacity, but all payload storage moved to
    /// the emitted packet. External retained outputs are releasable pressure.
    pub(crate) fn recovery_storage_bytes(&self) -> Result<usize> {
        debug_assert!(self.recovery_idle());
        debug_assert!(self.spans.is_empty());
        self.header_ranges
            .capacity()
            .checked_add(self.output_header.allocated())
            .and_then(|count| count.checked_mul(size_of::<SourceRange>()))
            .and_then(|bytes| {
                self.spans
                    .allocated()
                    .checked_mul(size_of::<DemuxSpan>())
                    .and_then(|spans| bytes.checked_add(spans))
            })
            .ok_or(Error::Overflow)
    }

    pub(crate) fn rejected_fault_input(
        &self,
        packet: &crate::transport::RejectedTransport,
        context: SourceContext,
    ) -> Result<&Budget> {
        self.validate_input(None)?;
        if !packet.bytes().belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if !self.recovery_idle() {
            return Err(Error::Invalid("selected diagnostic input not drained"));
        }
        if self.scope.stream_type != 0x1b
            || self.scope.stream_id != 0xe0
            || packet.source_id() != Some(self.scope.entity)
            || context.epochs != self.scope.epochs
            || context.byte_run_epoch.is_some()
        {
            return Err(Error::Unsupported(
                "selected diagnostic source context unobserved",
            ));
        }
        if !self.follows_input(context, packet.source_offset()) {
            return Err(Error::Invalid("native PES source event order"));
        }
        packet
            .source_offset()
            .checked_add(u64::try_from(packet.bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(&self.budget)
    }

    #[allow(
        clippy::result_large_err,
        reason = "return original charged transport input inline"
    )]
    fn push_boundary(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        selected_boundary: bool,
        input: Option<&triergon_core::InputCursor>,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        if let Some(error) = self.input_error(&packet, context, selected_boundary, input, false) {
            return Err(Refused {
                error,
                value: packet,
            });
        }
        self.retain_carrier(
            CarrierLayout::from(&packet),
            packet.source,
            packet.payload,
            context,
        );
        Ok(())
    }

    fn retain_carrier(
        &mut self,
        carrier: CarrierLayout,
        source: Bytes,
        payload: Option<Bytes>,
        context: SourceContext,
    ) {
        self.last_input = Some((context.source_event_ordinal, carrier.source_offset));
        self.entity_first = false;
        self.pending = Some(Pending {
            carrier,
            source,
            payload,
            context,
            position: 0,
            counter_checked: false,
            started: false,
            body_drained: false,
        });
    }

    pub(super) fn input_error(
        &self,
        packet: &TransportPacket,
        context: SourceContext,
        selected_boundary: bool,
        input: Option<&triergon_core::InputCursor>,
        transport_fault: bool,
    ) -> Option<Error> {
        if let Err(error) = self.validate_input(input) {
            Some(error)
        } else if !packet.source.belongs_to(&self.budget) {
            Some(Error::Invalid("foreign native PES byte pool"))
        } else if self.pending.is_some() || self.emission.is_some() {
            Some(Error::Invalid("native PES input not drained"))
        } else if self.ended {
            Some(Error::Invalid("native PES input after EOF"))
        } else if let Some(error) = self.fault {
            Some(error)
        } else if packet.pid != self.scope.pid {
            Some(Error::Invalid("native PES input PID"))
        } else if packet.continuity_counter > 15 {
            Some(Error::Invalid("native TS counter range"))
        } else if packet.source_id != Some(self.scope.entity) || context.epochs != self.scope.epochs
        {
            Some(Error::Unsupported(
                "native PES source context transition unobserved",
            ))
        } else if !transport_fault && (packet.transport_error || packet.scrambling != 0) {
            Some(Error::Unsupported(
                "native PES damaged/opaque transport unobserved",
            ))
        } else if self.finite_private() && packet.prefix.is_some() {
            Some(self.finite_private_error(
                "native private audio M2TS context unobserved",
                "native DVB M2TS context unobserved",
            ))
        } else if self.finite_private()
            && (context.byte_run_epoch.is_some() || packet.discontinuity_indicator())
        {
            Some(self.finite_private_error(
                "native private audio damaged/clock context unobserved",
                "native DVB damaged/clock context unobserved",
            ))
        } else if !selected_boundary
            && packet.discontinuity_indicator()
            && packet
                .adaptation
                .as_ref()
                .is_some_and(|field| field.pcr.is_some())
        {
            Some(Error::Unsupported(
                "native PCR clock boundary context unobserved",
            ))
        } else if !self.follows_input(context, packet.source_offset) {
            Some(Error::Invalid("native PES source event order"))
        } else {
            let Some(end) = packet.source_offset.checked_add(packet.source.len() as u64) else {
                return Some(Error::Overflow);
            };
            Self::transport_bounds_to(packet, Some(end)).err()
        }
    }

    pub(super) fn follows_input(&self, context: SourceContext, offset: u64) -> bool {
        self.last_input
            .is_none_or(|(previous_event, previous_offset)| {
                context.source_event_ordinal > previous_event
                    && (self.entity_first || offset > previous_offset)
            })
    }

    #[cfg(any(test, feature = "reference-trace"))]
    pub(crate) fn transport_bounds(packet: &TransportPacket) -> Result<()> {
        Self::transport_bounds_to(packet, None)
    }

    fn transport_bounds_to(packet: &TransportPacket, checked_end: Option<u64>) -> Result<()> {
        let prefix = packet.prefix.as_ref().map_or(0, Bytes::len);
        if (prefix != 0 && prefix != 4) || packet.source.len() != 188 + prefix {
            return Err(Error::Invalid("native TS packet layout"));
        }
        if let Some(payload) = packet.payload.as_ref() {
            let (start, end) = if let Some(end) = checked_end {
                // Admission checked the entire carrier end. The layout above
                // proves its prefix and header fit inside that same extent.
                (packet.source_offset + prefix as u64 + 4, end)
            } else {
                let start = packet
                    .source_offset
                    .checked_add(prefix as u64 + 4)
                    .ok_or(Error::Overflow)?;
                let end = packet
                    .source_offset
                    .checked_add(packet.source.len() as u64)
                    .ok_or(Error::Overflow)?;
                (start, end)
            };
            let payload_end = packet
                .payload_source_offset
                .checked_add(payload.len() as u64)
                .ok_or(Error::Overflow)?;
            if payload.len() > 184 || packet.payload_source_offset < start || payload_end > end {
                return Err(Error::Invalid("native TS payload bounds"));
            }
            let relative = usize::try_from(packet.payload_source_offset - packet.source_offset)
                .map_err(|_| Error::Overflow)?;
            if !packet
                .source
                .as_ref()
                .get(relative..relative + payload.len())
                .is_some_and(|span| std::ptr::eq(span, payload.as_ref()))
            {
                return Err(Error::Invalid("native TS payload bounds"));
            }
        }
        Ok(())
    }
}
