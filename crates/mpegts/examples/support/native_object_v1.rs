//! Full native object rendering; packet assembly stays in the existing observer.
use super::*;
use triergon_core::BudgetVec;
use triergon_mpegts::native_catalogue::NativeCatalogueV1;

struct Captured {
    packet: NativePesPacket,
    pts: Option<i128>,
    dts: Option<i128>,
}
struct Discard {
    bytes: Bytes,
    location: SourceLocation,
    pid: Option<u16>,
    error: Error,
}
pub(super) struct ObjectAdapter {
    catalogue: NativeCatalogueV1,
    budget: Budget,
    program: u16,
    packets: BudgetVec<Captured>,
    discards: BudgetVec<Discard>,
    trailing: Option<SourceLocation>,
    media: bool,
    rejected_counter: Option<(SourceLocation, u16, u8)>,
    counter_consequence: Option<(SourceLocation, u16)>,
    sync_rejected: bool,
    fault: Option<Error>,
    psi_compatibility: bool,
}
impl ObjectAdapter {
    pub(super) fn new(budget: Budget, program: u16) -> triergon_core::Result<Self> {
        Ok(Self {
            catalogue: NativeCatalogueV1::new(budget.clone(), ParseLimits::default(), program)?,
            packets: BudgetVec::new(&budget),
            discards: BudgetVec::new(&budget),
            budget,
            program,
            trailing: None,
            media: false,
            rejected_counter: None,
            counter_consequence: None,
            sync_rejected: false,
            fault: None,
            psi_compatibility: false,
        })
    }
    pub(super) fn new_psi_compatibility_v1(
        budget: Budget,
        program: u16,
        policies: triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1,
    ) -> triergon_core::Result<Self> {
        let mut object = Self::new(budget, program)?;
        object.catalogue.enable_psi_compatibility_v1(policies)?;
        object.psi_compatibility = true;
        Ok(object)
    }
    pub(super) fn new_selected_carriage_v2(
        budget: Budget,
        program: u16,
        format: PacketFormat,
    ) -> triergon_core::Result<Self> {
        let mut object = Self::new(budget.clone(), program)?;
        object.catalogue = NativeCatalogueV1::new_selected_carriage_v2(
            budget,
            ParseLimits::default(),
            program,
            format,
        )?;
        object.catalogue.enable_psi_compatibility_v1(
            triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1::ALL_OBSERVED,
        )?;
        object.psi_compatibility = true;
        Ok(object)
    }
    pub(super) fn packet_completion_ready_v1(&self) -> triergon_core::Result<()> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        if !self.catalogue.is_ended() {
            return Err(Error::Invalid(
                "packet completion catalogue has no actual physical End",
            ));
        }
        Ok(())
    }
    pub(super) fn uses_psi_compatibility_v1(&self) -> bool {
        self.psi_compatibility
    }
    pub(super) fn missing_psi_source_context_v1(&mut self) {
        self.fault = Some(Error::Invalid(
            "PSI compatibility actual source ordinal missing",
        ));
    }

    pub(super) fn observe_psi_context_v1(
        &mut self,
        event: &TraceEvent,
        idle: bool,
        source: SourceId,
        context: NativeSourceContext,
        cancel: &AtomicBool,
    ) -> bool {
        self.observe_bound_v1(event, idle, Some((source, context)), cancel)
    }
    pub(super) fn observe(&mut self, event: &TraceEvent, idle: bool, cancel: &AtomicBool) -> bool {
        self.observe_bound_v1(event, idle, None, cancel)
    }
    fn observe_bound_v1(
        &mut self,
        event: &TraceEvent,
        idle: bool,
        source: Option<(SourceId, NativeSourceContext)>,
        cancel: &AtomicBool,
    ) -> bool {
        if self.fault.is_some() {
            return false;
        }
        let result = self.observe_inner(event, idle, source, cancel);
        match result {
            Ok(skip) => skip,
            Err(e) => {
                self.fault = Some(e);
                false
            }
        }
    }
    fn discard(
        &mut self,
        bytes: &Bytes,
        location: SourceLocation,
        pid: Option<u16>,
        error: Error,
    ) -> triergon_core::Result<()> {
        if !bytes.belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if self.discards.len() >= 4096 {
            return Err(Error::Unsupported("native object discard evidence limit"));
        }
        self.discards
            .try_push(Discard {
                bytes: bytes.clone(),
                location,
                pid,
                error,
            })
            .map_err(|e| e.error)
    }
    fn observe_inner(
        &mut self,
        event: &TraceEvent,
        idle: bool,
        source: Option<(SourceId, NativeSourceContext)>,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<bool> {
        if !self.catalogue.is_ended() && self.psi_compatibility {
            let (source, context) = source.ok_or(Error::Invalid(
                "PSI compatibility actual source context missing",
            ))?;
            let cost = self.catalogue.required_psi_compatibility_work_v1(event)?;
            if !self.catalogue.observe_psi_compatibility_v1(
                event,
                source,
                context,
                &mut WorkBudget::new(cost),
                cancel,
            )? {
                return Err(Error::Invalid(
                    "PSI compatibility exact source grant yielded",
                ));
            }
        } else if !self.catalogue.is_ended() {
            let cost = NativeCatalogueV1::required_work(event)?;
            if !self
                .catalogue
                .observe(event, &mut WorkBudget::new(cost), cancel)?
            {
                return Err(Error::Invalid(
                    "native catalogue exact syntax grant yielded",
                ));
            }
        } else if !matches!(event, TraceEvent::Finished | TraceEvent::Diagnostic(_)) {
            return Err(Error::Invalid("native object data after physical end"));
        }
        match event {
            TraceEvent::Transport(p) => {
                if self.sync_rejected {
                    return Err(Error::Unsupported(
                        "native post-sync resynchronization context unobserved",
                    ));
                }
                let Some(body) = p.payload.as_ref() else {
                    return Ok(false);
                };
                // The native worker's object includes every native program. A
                // selected-source map cannot stand in for another program or SI.
                if p.pid != 0
                    && p.pid < 0x20
                    && !body.is_empty()
                    && !self
                        .catalogue
                        .programs()
                        .iter()
                        .any(|route| route.pmt_pid == p.pid)
                    && !self
                        .catalogue
                        .streams()
                        .iter()
                        .any(|stream| stream.pid == p.pid)
                {
                    return Err(Error::Unsupported(
                        "native broadcast-SI catalogue requires an explicit source adapter",
                    ));
                }
                if self
                    .catalogue
                    .programs()
                    .iter()
                    .any(|route| route.program != self.program && route.pmt_pid == p.pid)
                    && !body.is_empty()
                {
                    return Err(Error::Unsupported(
                        "native other-program PMT catalogue requires an explicit source adapter",
                    ));
                }
                if self.catalogue.has_pending_current_pat()
                    && !self.catalogue.has_psi_compatibility_route_v1(p.pid)
                    && p.pid != 0
                    && p.payload_unit_start
                {
                    let at = body.as_ref().first().map(|b| usize::from(*b) + 1);
                    if at.and_then(|i| body.as_ref().get(i)).copied() == Some(2) {
                        return Err(Error::Unsupported(
                            "native early PMT before complete source PAT requires compatibility review",
                        ));
                    }
                }
                if self.catalogue.streams().iter().any(|s| s.pid == p.pid) {
                    if let Some((old, pid, counter)) = self.rejected_counter.take() {
                        if p.source_id != Some(old.source)
                            || p.source_offset <= old.offset
                            || p.pid != pid
                            || !p.payload_unit_start
                            || p.continuity_counter != (counter + 1) & 15
                            || body.len() < 9
                        {
                            return Err(Error::Unsupported(
                                "native rejected-carrier continuation outside observed finite-PUSI boundary",
                            ));
                        }
                        let raw = body.as_ref();
                        let finite = usize::from(u16::from_be_bytes([raw[4], raw[5]]));
                        if raw[..3] != [0, 0, 1]
                            || finite == 0
                            || finite + 6 != body.len()
                            || usize::from(raw[8]) + 9 > body.len()
                        {
                            return Err(Error::Unsupported(
                                "native rejected-carrier continuation requires one complete finite PES",
                            ));
                        }
                        self.counter_consequence = Some((
                            SourceLocation {
                                source: old.source,
                                offset: p.source_offset,
                            },
                            p.pid,
                        ));
                    }
                    if p.payload_unit_start
                        && idle
                        && body.len() >= 9
                        && p.prefix.is_none()
                        && p.source.len() == 188
                    {
                        let b = body.as_ref();
                        let error = if b[..3] != [0, 0, 1] {
                            Some(Error::Invalid("PES start code"))
                        } else {
                            let length = usize::from(u16::from_be_bytes([b[4], b[5]]));
                            (length != 0 && length < 3 + usize::from(b[8]))
                                .then_some(Error::Invalid("PES header exceeds length"))
                        };
                        if let Some(error) = error {
                            self.discard(
                                &p.source,
                                SourceLocation {
                                    source: p.source_id.ok_or(Error::SourceMismatch)?,
                                    offset: p.source_offset,
                                },
                                Some(p.pid),
                                error,
                            )?;
                            return Ok(true);
                        }
                    }
                    if !body.is_empty() {
                        self.media = true;
                    }
                } else if p.payload_unit_start && body.as_ref().starts_with(&[0, 0, 1]) {
                    return Err(Error::Unsupported(
                        "native object unrouted PES requires independent scope",
                    ));
                }
            }
            TraceEvent::Diagnostic(f) if f.kind == FaultKind::Transport => {
                if !idle {
                    return Err(Error::Unsupported(
                        "native object rejected carrier during active PES",
                    ));
                }
                if let Some(r) = f.rejected.as_ref() {
                    let b = r.bytes().as_ref();
                    if r.source_id() != Some(f.location.source)
                        || r.source_offset() != f.location.offset
                        || r.error() != f.error
                        || b.len() != 188
                        || f.location.offset % 188 != 0
                    {
                        return Err(Error::Invalid(
                            "native object rejected carrier witness mismatch",
                        ));
                    }
                    let unusable = match f.error {
                        Error::Invalid("TS sync byte") => b[0] != 0x47,
                        Error::Invalid("reserved adaptation control") => {
                            b[0] == 0x47 && (b[3] >> 4) & 3 == 0
                        }
                        Error::Invalid("adaptation length/control") => {
                            b[0] == 0x47 && (b[3] >> 4) & 3 == 3 && usize::from(b[4]) > 182
                        }
                        _ => false,
                    };
                    if !unusable {
                        return Err(Error::Unsupported(
                            "native object rejected carrier policy unobserved",
                        ));
                    }
                    if b[0] == 0x47 {
                        let pid = u16::from_be_bytes([b[1] & 31, b[2]]);
                        if self.catalogue.streams().iter().any(|s| s.pid == pid) {
                            self.rejected_counter = Some((f.location, pid, b[3] & 15));
                        }
                    } else {
                        self.sync_rejected = true;
                    }
                    self.discard(r.bytes(), f.location, None, f.error)?;
                    return Ok(true);
                } else if f.error == Error::Truncated && f.location.offset % 188 == 0 {
                    if self.trailing.replace(f.location).is_some() {
                        return Err(Error::Invalid("native object repeated truncated tail"));
                    }
                    return Ok(true);
                }
                return Err(Error::Unsupported(
                    "native object transport diagnostic unobserved",
                ));
            }
            TraceEvent::Diagnostic(f)
                if f.kind == FaultKind::Pes
                    && matches!(
                        f.error,
                        Error::Invalid("PES start code" | "PES header exceeds length")
                    ) =>
            {
                if self.discards.as_slice().iter().any(|d| {
                    d.error == f.error
                        && d.pid == f.pid
                        && d.location.source == f.location.source
                        && d.location.offset <= f.location.offset
                        && f.location.offset < d.location.offset + 188
                }) {
                    return Ok(true);
                }
                return Err(Error::Invalid("native object missing unusable PES witness"));
            }
            TraceEvent::Diagnostic(f)
                if f.kind == FaultKind::Continuity
                    && f.error == Error::Invalid("TS continuity loss") =>
            {
                // Strict still reports a lost counter because its rejected carrier
                // had no source header. The separate native finite-PUSI view is
                // admitted only by the real encoded counter and following bytes.
                if self.counter_consequence.is_some_and(|(at, pid)| {
                    f.pid == Some(pid)
                        && f.location.source == at.source
                        && at.offset <= f.location.offset
                        && f.location.offset < at.offset + 188
                }) {
                    self.counter_consequence = None;
                    return Ok(true);
                }
                return Err(Error::Unsupported(
                    "native object source continuity context unobserved",
                ));
            }
            TraceEvent::Diagnostic(f)
                if matches!(f.kind, FaultKind::Continuity | FaultKind::Program) =>
            {
                return Err(Error::Unsupported(
                    "native object source continuity/program diagnostic unobserved",
                ));
            }
            _ => {}
        }
        Ok(false)
    }
    pub(super) fn end(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &InputCursor,
    ) -> triergon_core::Result<()> {
        self.end_bound_v1(end, input, None)
    }
    pub(super) fn end_psi_with_cancel_v1(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &InputCursor,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        self.end_bound_v1(end, input, Some(cancel))
    }
    fn end_bound_v1(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &InputCursor,
        cancel: Option<&AtomicBool>,
    ) -> triergon_core::Result<()> {
        if let Some(e) = self.fault {
            return Err(e);
        }
        if let Some(tail) = self.trailing {
            let length = end
                .location()
                .offset
                .checked_sub(tail.offset)
                .ok_or(Error::Overflow)?;
            if end.location().source != tail.source || !(1..188).contains(&length) {
                self.fault = Some(Error::Invalid(
                    "native object tail lacks real short terminal range",
                ));
                return Err(self.fault.unwrap());
            }
        }
        let result = if self.psi_compatibility {
            let cancel = cancel.ok_or(Error::Invalid(
                "PSI compatibility terminal cancellation context missing",
            ))?;
            let cost = self.catalogue.required_psi_compatibility_finish_work_v1()?;
            self.catalogue
                .finish_psi_compatibility_v1(end, input, &mut WorkBudget::new(cost), cancel)
                .and_then(|done| {
                    if done {
                        Ok(())
                    } else {
                        Err(Error::Invalid("PSI compatibility terminal grant yielded"))
                    }
                })
        } else {
            self.catalogue.finish(end, input)
        };
        if let Err(e) = result {
            self.fault = Some(e);
        }
        result
    }
    pub(super) fn capture(&mut self, packet: NativePesPacket, time: &NativePesTimeView) {
        if time.pts90.is_some_and(|t| i64::try_from(t).is_err())
            || time.dts90.is_some_and(|t| i64::try_from(t).is_err())
        {
            self.fault = Some(Error::Overflow);
            return;
        }
        if let Err(e) = self.packets.try_push(Captured {
            packet,
            pts: time.pts90,
            dts: time.dts90,
        }) {
            self.fault = Some(e.error);
        }
    }
    pub(super) fn unsupported_packet(&mut self) {
        self.fault = Some(Error::Unsupported(
            "native whole object diagnostic packet profile unobserved",
        ));
    }
    pub(super) fn error(&self) -> Option<Error> {
        self.fault.or(self.catalogue.fault())
    }
    pub(super) fn json(
        &self,
        output: &mut impl Write,
        failed: bool,
        finished: bool,
        packets: u64,
    ) -> io::Result<bool> {
        if self.error().is_some()
            || !self.catalogue.is_ended()
            || (self.media && (failed || !finished || packets != self.packets.len() as u64))
        {
            return Ok(false);
        }
        write!(
            output,
            "{{\"schema\":1,\"operation\":\"mpegts-native-pes-observation-v1\",\"program\":{},\"packets\":[",
            self.program
        )?;
        for (i, c) in self.packets.as_slice().iter().enumerate() {
            if i != 0 {
                output.write_all(b",")?;
            }
            let p = &c.packet;
            let index = self
                .catalogue
                .streams()
                .iter()
                .position(|s| s.pid == p.pid)
                .ok_or_else(|| io::Error::other("captured native stream missing"))?;
            write!(output, "{{\"stream\":{index},\"pid\":{},\"pts\":", p.pid)?;
            source_trace::signed_optional(output, c.pts)?;
            output.write_all(b",\"dts\":")?;
            source_trace::signed_optional(output, c.dts)?;
            write!(
                output,
                ",\"duration\":{},\"position\":{},\"flags\":{},\"payload\":\"",
                p.native_duration90_raw, p.native_position, p.native_flags
            )?;
            for span in p.payload() {
                for b in span {
                    write!(output, "{b:02x}")?;
                }
            }
            write!(
                output,
                "\",\"side_data\":[{{\"type\":19,\"bytes\":\"{:02x}\"}}]}}",
                p.native_stream_id
            )?;
        }
        output.write_all(b"],\"streams\":[")?;
        for (i, s) in self.catalogue.streams().iter().enumerate() {
            if i != 0 {
                output.write_all(b",")?;
            }
            let codec = s.native_codec_v2();
            write!(
                output,
                "{{\"stream\":{i},\"pid\":{},\"codec\":\"{}\",\"type\":{},\"time_base\":[1,90000],\"extradata\":\"\",\"tags\":[",
                s.pid,
                codec.name(),
                codec.media_type()
            )?;
            if !s.language.is_empty() {
                output.write_all(b"{\"key_hex\":\"6c616e6775616765\",\"value_hex\":")?;
                source_trace::hex(output, s.language.as_slice())?;
                output.write_all(b"}")?;
            }
            output.write_all(b"]}")?;
        }
        output.write_all(b"],\"programs\":[")?;
        for (i, p) in self.catalogue.programs().iter().enumerate() {
            if i != 0 {
                output.write_all(b",")?;
            }
            write!(
                output,
                "{{\"id\":{},\"program\":{},\"pmt_pid\":{},\"pcr_pid\":{},\"tags\":[]}}",
                p.program, p.program, p.pmt_pid, p.native_pcr_pid
            )?;
        }
        output.write_all(b"],\"read_result\":-541478725,\"confirmed_eof\":true}")?;
        Ok(true)
    }
    pub(super) fn evidence_json(&self, output: &mut impl Write) -> io::Result<()> {
        write!(
            output,
            "{{\"kind\":\"native_object_evidence\",\"view\":\"ArchivedNativeWholeObjectV1\",\"metadata_tables\":{},\"discarded_carriers\":[",
            self.catalogue.evidence().len()
        )?;
        for (i, d) in self.discards.as_slice().iter().enumerate() {
            if i != 0 {
                output.write_all(b",")?;
            }
            write!(
                output,
                "{{\"source\":{},\"offset\":{},\"bytes\":{},\"source_error\":",
                d.location.source.0,
                d.location.offset,
                d.bytes.len()
            )?;
            triergon_harness_protocol::write_quoted(output, &d.error.to_string())?;
            output.write_all(b",\"raw_hex\":")?;
            source_trace::hex(output, d.bytes.as_ref())?;
            output.write_all(b"}")?;
        }
        output.write_all(b"],\"trailing_source_offset\":")?;
        source_trace::optional(output, self.trailing.map(|t| t.offset))?;
        output.write_all(b",\"source_acceptance_unchanged\":true,\"qualified\":false}\n")?;
        for record in self.catalogue.psi_compatibility_evidence_v1() {
            let receipt = record.receipt();
            let header = record.wire_header();
            write!(
                output,
                "{{\"kind\":\"native_psi_compatibility\",\"policy\":\"{:?}\",\"source_event_ordinal\":{},\"source_clock_epoch\":{},\"source_configuration_epoch\":{},\"wire_section_number\":{},\"wire_last_section_number\":{},\"wire_version\":{},\"source_status\":",
                receipt.policy,
                receipt.context.source_event_ordinal,
                receipt.context.epochs.clock,
                receipt.context.epochs.configuration,
                header.section_number,
                header.last_section_number,
                header.version
            )?;
            triergon_harness_protocol::write_quoted(
                output,
                &format!("{:?}", record.source_status()),
            )?;
            output.write_all(b",\"section_hex\":")?;
            source_trace::hex(output, record.section_bytes().as_ref())?;
            output.write_all(b",\"source_ranges\":[")?;
            for (index, range) in record.source_ranges().iter().enumerate() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                range_json(output, *range)?;
            }
            output.write_all(b"],\"descriptor_stop_offset\":")?;
            source_trace::optional(output, record.descriptor_stop_offset().map(|n| n as u64))?;
            output.write_all(b",\"actual_carriers\":[")?;
            for index in 0..record.carrier_count() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                let (packet, context) = record
                    .carrier(index)
                    .ok_or_else(|| io::Error::other("PSI compatibility carrier proof missing"))?;
                write!(
                    output,
                    "{{\"source\":{},\"offset\":{},\"bytes\":{},\"source_event_ordinal\":{},\"raw_hex\":",
                    packet
                        .source_id
                        .ok_or_else(|| io::Error::other("PSI compatibility source ID missing"))?
                        .0,
                    packet.source_offset,
                    packet.source.len(),
                    context.source_event_ordinal
                )?;
                source_trace::hex(output, packet.source.as_ref())?;
                output.write_all(b"}")?;
            }
            output.write_all(b"],\"source_acceptance_unchanged\":true,\"qualified\":false}\n")?;
        }
        Ok(())
    }
}
