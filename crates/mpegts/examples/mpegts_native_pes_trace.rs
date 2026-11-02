//! Composed strict-source trace plus the explicit archived native PES operation.
//! Diagnostic JSON/CLI/I/O are outside timing. No native library is linked.
#![forbid(unsafe_code)]

// Reuse the checked source renderer verbatim. Its standalone CLI stays separate.
#[path = "mpegts_source_trace.rs"]
#[allow(
    dead_code,
    reason = "the nested source example retains its standalone CLI"
)]
mod source_trace;

use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    SourceLocation, Step, WorkBudget,
};
use triergon_mpegts::clock::{NativeClockProfile, NativePesClockV1, NativePesTimeView};
use triergon_mpegts::native_pes::{
    NativeDvbSubtitlesDescriptors, NativePesPacket, NativePesScope, NativePrivateAudioCarriage,
    NativePrivateAudioDescriptors, NativeRegisteredCarriage, NativeSourceContext,
};
use triergon_mpegts::native_recovery::{
    NativeDiagnosticPesPacket, NativePesObservation, NativePesRecoveryObserver,
    NativeRecoveredPesPacket, NativeRejectedPolicy,
};
use triergon_mpegts::psi::{DescriptorNamespace, Descriptors, DvbNamespaceEvidence};
use triergon_mpegts::{
    AssemblyStep, FaultKind, PacketFormat, ParseLimits, RejectedTransport, SourceEpochs,
    SourceRange, TraceEvent, TransportStream,
};

const PROFILE: NativeClockProfile = NativeClockProfile::ArchivedPesV1;

fn diagnostic_packet_json(
    output: &mut impl Write,
    p: &NativeDiagnosticPesPacket,
    n: u64,
    cause: Cause,
    time: &triergon_core::Result<triergon_mpegts::clock::NativeDiagnosticTimeView>,
    budget: &Budget,
) -> io::Result<()> {
    write!(
        output,
        "{{\"kind\":\"native_pes\",\"operation\":\"ArchivedNativePesPacketsV1\",\"profile\":\"{:?}\",\"source_fault_view\":\"NativeD32SelectedAvcDiagnosticsV1\",\"native_packet_ordinal\":{n},\"program\":{},\"pid\":{},\"native_stream_index\":0,\"native_time_base\":[1,90000],\"source_origin\":{{\"source\":{},\"offset\":{}}},\"source_clock_epoch\":{},\"source_configuration_epoch\":{},\"native_position\":{},\"native_flags\":{},\"native_duration90_raw\":{},\"native_stream_id\":{},\"boundary\":\"FiniteLength\",\"payload_kind\":\"DeclaredComplete\",\"source_acceptance\":\"rejected\",\"source_header_status\":\"NotAcceptedBecauseSourceDiagnostic\",\"source_header_hex\":null,\"source_pts_raw33\":null,\"source_dts_raw33\":null,\"time_header_applies\":false,\"native_encoded_header_applies\":true,\"selected_fault\":\"{:?}\",\"native_encoded_header_hex\":",
        p.profile,
        p.program,
        p.pid,
        p.source_origin.source.0,
        p.source_origin.offset,
        p.source_epochs.clock,
        p.source_epochs.configuration,
        p.native_position,
        p.native_flags,
        p.native_duration90_raw,
        p.native_stream_id,
        p.selected_fault
    )?;
    source_trace::hex(output, p.encoded_header.as_bytes())?;
    output.write_all(b",\"native_encoded_pts_raw33\":")?;
    source_trace::optional(output, p.encoded_header.encoded_pts_raw33())?;
    write!(
        output,
        ",\"native_encoded_dts_raw33\":null,\"encoded_markers_valid\":{},\"encoded_time_flags\":{},\"source_diagnostic\":{{\"layer\":\"{:?}\",\"error\":",
        p.encoded_header.markers_valid(),
        p.encoded_header.time_flags(),
        p.source_fault.kind
    )?;
    source_trace::json_string(output, &format!("{:?}", p.source_fault.error))?;
    output.write_all(b",\"pid\":")?;
    source_trace::optional(output, p.source_fault.pid.map(u64::from))?;
    write!(
        output,
        ",\"source\":{},\"offset\":{},\"source_event_ordinal\":{}}},\"raw_carrier\":{{\"source\":{},\"offset\":{},\"bytes\":188,\"owner_retained_capacity\":{},\"owner_storage_bytes\":{},\"raw_hex\":",
        p.source_fault.location.source.0,
        p.source_fault.location.offset,
        p.diagnostic_context.source_event_ordinal,
        p.source_origin.source.0,
        p.carrier.source_offset(),
        p.carrier.backing_bytes().retained_capacity(),
        p.carrier.backing_bytes().retained_storage_bytes()
    )?;
    source_trace::hex(output, p.carrier.backing_bytes().as_ref())?;
    output.write_all(b"}")?;
    if let triergon_mpegts::native_recovery::NativeDiagnosticCarrier::Rejected(r) = &p.carrier {
        output.write_all(b",\"rejected_carrier\":")?;
        rejection_json(output, r)?;
    }
    output.write_all(b",\"header_source_ranges\":[")?;
    for (i, r) in p.header_source_ranges.as_slice().iter().enumerate() {
        if i != 0 {
            output.write_all(b",")?;
        }
        range_json(output, *r)?;
    }
    write!(
        output,
        "],\"payload_bytes\":{},\"payload_hex\":\"",
        p.payload_len
    )?;
    for span in p.payload() {
        for byte in span {
            write!(output, "{byte:02x}")?;
        }
    }
    output.write_all(b"\",\"spans\":[")?;
    for (i, span) in p.spans.as_slice().iter().enumerate() {
        if i != 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"{\"source_range\":")?;
        range_json(output, span.source)?;
        output.write_all(b",\"transport_range\":")?;
        range_json(output, span.transport)?;
        write!(
            output,
            ",\"source_event_kind\":\"{}\",\"source_event_ordinal\":{},\"clock_epoch\":{},\"configuration_epoch\":{},\"byte_run_epoch\":null,\"owner_retained_capacity\":{},\"owner_storage_bytes\":{}}}",
            if p.source_fault.rejected.is_some() {
                "diagnostic"
            } else {
                "transport"
            },
            span.context.source_event_ordinal,
            span.context.epochs.clock,
            span.context.epochs.configuration,
            span.backing_bytes().retained_capacity(),
            span.backing_bytes().retained_storage_bytes()
        )?;
    }
    output.write_all(b"],\"native_side_data\":[{\"type\":19,\"hex\":\"e0\"}],\"native_time\":")?;
    match time {
        Ok(t) => {
            output.write_all(b"{\"applicable_source_pts_raw33\":null,\"applicable_source_dts_raw33\":null,\"encoded_pts_raw33\":")?;
            source_trace::optional(output, t.encoded_pts_raw33)?;
            output.write_all(b",\"encoded_dts_raw33\":null,\"pts90\":")?;
            source_trace::signed_optional(output, t.pts90)?;
            output.write_all(b",\"dts90\":")?;
            source_trace::signed_optional(output, t.dts90)?;
            write!(
                output,
                ",\"pts_origin\":\"{}\",\"dts_origin\":\"{}\",\"first_reference_raw90\":",
                if t.pts90.is_some() {
                    "EncodedDiagnosticBits"
                } else {
                    "Absent"
                },
                if t.dts90.is_some() {
                    "PtsOnlyCompatibility"
                } else {
                    "Absent"
                }
            )?;
            source_trace::optional(output, t.origin.map(|o| o.first_reference_raw90))?;
            output.write_all(b",\"interval_origin90\":")?;
            source_trace::signed_optional(output, t.origin.map(|o| o.interval_origin90))?;
            output.write_all(b"}")?;
        }
        Err(error) => {
            output.write_all(b"{\"error\":")?;
            source_trace::json_string(output, &error.to_string())?;
            output.write_all(b"}")?;
        }
    }
    output.write_all(b",\"emission_cause\":")?;
    cause_json(output, cause)?;
    writeln!(
        output,
        ",\"shared_budget_used\":{},\"shared_budget_peak\":{},\"qualified\":false}}",
        budget.used(),
        budget.peak()
    )
}

fn rejection_json(output: &mut impl Write, rejection: &RejectedTransport) -> io::Result<()> {
    write!(
        output,
        "{{\"source\":{},\"offset\":{},\"bytes\":{},\"error\":",
        rejection.source_id().map_or(0, |s| s.0),
        rejection.source_offset(),
        rejection.bytes().len()
    )?;
    source_trace::json_string(output, &format!("{:?}", rejection.error()))?;
    output.write_all(b",\"secondary_error\":")?;
    if let Some(error) = rejection.secondary_error() {
        source_trace::json_string(output, &format!("{error:?}"))?;
    } else {
        output.write_all(b"null")?;
    }
    write!(
        output,
        ",\"fully_checked\":{},\"owner_retained_capacity\":{},\"owner_storage_bytes\":{},\"raw_hex\":",
        rejection.fully_checked(),
        rejection.backing_bytes().retained_capacity(),
        rejection.backing_bytes().retained_storage_bytes()
    )?;
    source_trace::hex(output, rejection.bytes().as_ref())?;
    output.write_all(b"}")
}

fn recovered_json(
    output: &mut impl Write,
    packet: &NativeRecoveredPesPacket,
    ordinal: u64,
    cause: Cause,
    time: &triergon_core::Result<triergon_mpegts::clock::NativeEncodedTimeView>,
    budget: &Budget,
) -> io::Result<()> {
    write!(
        output,
        "{{\"kind\":\"native_pes\",\"operation\":\"ArchivedNativePesPacketsV1\",\"profile\":\"{:?}\",\"native_packet_ordinal\":{ordinal},\"program\":{},\"pid\":{},\"native_stream_index\":0,\"native_time_base\":[1,90000],\"source_origin\":{{\"source\":{},\"offset\":{}}},\"source_clock_epoch\":{},\"source_configuration_epoch\":{},\"native_position\":{},\"native_flags\":{},\"native_duration90_raw\":{},\"native_stream_id\":{},\"boundary\":\"FiniteLength\",\"payload_kind\":\"DeclaredComplete\",\"source_acceptance\":\"rejected\",\"source_header_status\":\"NotAcceptedBecauseCarrierRejected\",\"source_header_hex\":null,\"source_pts_raw33\":null,\"source_dts_raw33\":null,\"time_header_applies\":false,\"native_encoded_header_applies\":true,\"native_encoded_header_hex\":",
        packet.profile,
        packet.program,
        packet.pid,
        packet.source_origin.source.0,
        packet.source_origin.offset,
        packet.source_epochs.clock,
        packet.source_epochs.configuration,
        packet.native_position,
        packet.native_flags,
        packet.native_duration90_raw,
        packet.native_stream_id
    )?;
    source_trace::hex(output, packet.encoded_header.as_bytes())?;
    write!(
        output,
        ",\"native_encoded_pts_raw33\":{},\"native_encoded_dts_raw33\":null,\"header_source_ranges\":[",
        packet.encoded_header.encoded_pts_raw33()
    )?;
    for (index, range) in packet.header_source_ranges.as_slice().iter().enumerate() {
        if index != 0 {
            output.write_all(b",")?;
        }
        range_json(output, *range)?;
    }
    write!(
        output,
        "],\"payload_bytes\":{},\"payload_hex\":\"",
        packet.payload_len
    )?;
    for part in packet.payload() {
        for byte in part {
            write!(output, "{byte:02x}")?;
        }
    }
    output.write_all(b"\",\"spans\":[")?;
    for (index, span) in packet.spans.as_slice().iter().enumerate() {
        if index != 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"{\"source_range\":")?;
        range_json(output, span.source)?;
        output.write_all(b",\"transport_range\":")?;
        range_json(output, span.transport)?;
        write!(
            output,
            ",\"source_event_kind\":\"diagnostic\",\"source_event_ordinal\":{},\"clock_epoch\":{},\"configuration_epoch\":{},\"byte_run_epoch\":null,\"owner_retained_capacity\":{},\"owner_storage_bytes\":{}}}",
            span.context.source_event_ordinal,
            span.context.epochs.clock,
            span.context.epochs.configuration,
            span.backing_bytes().retained_capacity(),
            span.backing_bytes().retained_storage_bytes()
        )?;
    }
    output.write_all(b"],\"native_side_data\":[{\"type\":19,\"hex\":\"e0\"}],\"native_time\":")?;
    match time {
        Ok(time) => write!(
            output,
            "{{\"applicable_source_pts_raw33\":null,\"applicable_source_dts_raw33\":null,\"encoded_pts_raw33\":{},\"encoded_dts_raw33\":null,\"pts90\":{},\"dts90\":{},\"pts_origin\":\"EncodedPts\",\"dts_origin\":\"PtsOnlyCompatibility\",\"first_reference_raw90\":{},\"interval_origin90\":{}}}",
            time.encoded_pts_raw33,
            time.pts90,
            time.dts90,
            time.origin.first_reference_raw90,
            time.origin.interval_origin90
        )?,
        Err(error) => {
            output.write_all(b"{\"error\":")?;
            source_trace::json_string(output, &error.to_string())?;
            output.write_all(b"}")?;
        }
    }
    output.write_all(b",\"rejected_carrier\":")?;
    rejection_json(output, &packet.rejection)?;
    output.write_all(b",\"emission_cause\":")?;
    cause_json(output, cause)?;
    writeln!(
        output,
        ",\"shared_budget_used\":{},\"shared_budget_peak\":{},\"qualified\":false}}",
        budget.used(),
        budget.peak()
    )
}

/// Ordinals count only original source rows, starting at zero. Entity/offset and
/// format epochs remain independent of core range-request tokens/generations.
#[derive(Clone, Copy)]
struct Cause {
    phase: &'static str,
    ordinal: Option<u64>,
    source: SourceId,
    offset: Option<u64>,
    epochs: SourceEpochs,
}
impl Cause {
    fn event(event: &TraceEvent, ordinal: u64, entity: SourceId, epochs: SourceEpochs) -> Self {
        let phase = match event {
            TraceEvent::Transport(_) => "transport",
            TraceEvent::Continuity { .. } => "continuity",
            TraceEvent::Section(_) => "section",
            TraceEvent::Pat(_) => "pat",
            TraceEvent::ProgramMap { .. } => "pmt",
            TraceEvent::Pes(_) => "pes",
            TraceEvent::Timeline { .. } => "timeline",
            TraceEvent::Discontinuity { .. } => "discontinuity",
            TraceEvent::Diagnostic(_) => "diagnostic",
            TraceEvent::Finished => "finished",
        };
        let mut cause = Self {
            phase,
            ordinal: Some(ordinal),
            source: entity,
            offset: None,
            epochs,
        };
        let range = match event {
            TraceEvent::Section(section) => section.source_ranges().first().copied(),
            TraceEvent::Pat(table) => table
                .source_ranges
                .as_slice()
                .first()
                .map(|item| item.range),
            TraceEvent::ProgramMap { map, .. } => map.source_ranges.as_slice().first().copied(),
            TraceEvent::Pes(pes) => pes.header_source_ranges.as_slice().first().copied(),
            _ => None,
        };
        if let Some(range) = range {
            cause.source = range.entity;
            cause.offset = Some(range.offset);
        }
        match event {
            TraceEvent::Transport(packet) => {
                cause.source = packet.source_id.unwrap_or(entity);
                cause.offset = Some(packet.source_offset);
            }
            TraceEvent::Diagnostic(fault) => {
                cause.source = fault.location.source;
                cause.offset = Some(fault.location.offset);
            }
            _ => {}
        }
        cause
    }
    fn boundary(phase: &'static str, location: SourceLocation, epochs: SourceEpochs) -> Self {
        Self {
            phase,
            ordinal: None,
            source: location.source,
            offset: Some(location.offset),
            epochs,
        }
    }
}

fn cause_json(output: &mut impl Write, cause: Cause) -> io::Result<()> {
    write!(
        output,
        "{{\"phase\":\"{}\",\"source_event_ordinal\":",
        cause.phase
    )?;
    source_trace::optional(output, cause.ordinal)?;
    write!(output, ",\"source\":{},\"offset\":", cause.source.0)?;
    source_trace::optional(output, cause.offset)?;
    write!(
        output,
        ",\"clock_epoch\":{},\"configuration_epoch\":{}}}",
        cause.epochs.clock, cause.epochs.configuration
    )
}
fn range_json(output: &mut impl Write, range: SourceRange) -> io::Result<()> {
    write!(
        output,
        "{{\"source\":{},\"offset\":{},\"bytes\":{}}}",
        range.entity.0, range.offset, range.bytes
    )
}
fn time_json(output: &mut impl Write, view: &NativePesTimeView) -> io::Result<()> {
    output.write_all(b"{\"applicable_source_pts_raw33\":")?;
    source_trace::optional(output, view.source_pts_raw33)?;
    output.write_all(b",\"applicable_source_dts_raw33\":")?;
    source_trace::optional(output, view.source_dts_raw33)?;
    output.write_all(b",\"pts90\":")?;
    source_trace::signed_optional(output, view.pts90)?;
    output.write_all(b",\"dts90\":")?;
    source_trace::signed_optional(output, view.dts90)?;
    write!(
        output,
        ",\"pts_origin\":\"{:?}\",\"dts_origin\":\"{:?}\",\"first_reference_raw90\":",
        view.pts_origin, view.dts_origin
    )?;
    source_trace::optional(
        output,
        view.origin.map(|origin| origin.first_reference_raw90),
    )?;
    output.write_all(b",\"interval_origin90\":")?;
    source_trace::signed_optional(output, view.origin.map(|origin| origin.interval_origin90))?;
    output.write_all(b"}")
}

fn packet_json(
    output: &mut impl Write,
    packet: &NativePesPacket,
    ordinal: u64,
    cause: Cause,
    time: &triergon_core::Result<NativePesTimeView>,
    budget: &Budget,
) -> io::Result<()> {
    write!(
        output,
        "{{\"kind\":\"native_pes\",\"operation\":\"ArchivedNativePesPacketsV1\",\"profile\":\"{:?}\",\"native_packet_ordinal\":{ordinal},\"program\":{},\"pid\":{},\"native_stream_index\":0,\"native_time_base\":[1,90000],\"source_origin\":{{\"source\":{},\"offset\":{}}},\"source_clock_epoch\":{},\"source_configuration_epoch\":{},\"native_position\":{},\"native_flags\":{},\"native_duration90_raw\":{},\"native_stream_id\":{},\"boundary\":\"{:?}\",\"payload_kind\":\"{:?}\",\"time_header_applies\":{},\"source_header_hex\":",
        packet.profile,
        packet.program,
        packet.pid,
        packet.source_origin.source.0,
        packet.source_origin.offset,
        packet.source_epochs.clock,
        packet.source_epochs.configuration,
        packet.native_position,
        packet.native_flags,
        packet.native_duration90_raw,
        packet.native_stream_id,
        packet.boundary,
        packet.payload_kind,
        packet.time_header_applies
    )?;
    source_trace::hex(output, packet.source_header.as_bytes())?;
    output.write_all(b",\"source_pts_raw33\":")?;
    source_trace::optional(output, packet.source_header.pts_raw33)?;
    output.write_all(b",\"source_dts_raw33\":")?;
    source_trace::optional(output, packet.source_header.dts_raw33)?;
    output.write_all(b",\"header_source_ranges\":[")?;
    for (index, range) in packet.header_source_ranges.as_slice().iter().enumerate() {
        if index != 0 {
            output.write_all(b",")?;
        }
        range_json(output, *range)?;
    }
    write!(
        output,
        "],\"payload_bytes\":{},\"payload_hex\":\"",
        packet.payload_len
    )?;
    for span in packet.payload() {
        for byte in span {
            write!(output, "{byte:02x}")?;
        }
    }
    output.write_all(b"\",\"spans\":[")?;
    for (index, span) in packet.spans.as_slice().iter().enumerate() {
        if index != 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"{\"source_range\":")?;
        range_json(output, span.source)?;
        output.write_all(b",\"transport_range\":")?;
        range_json(output, span.transport)?;
        write!(
            output,
            ",\"source_event_ordinal\":{},\"clock_epoch\":{},\"configuration_epoch\":{},\"byte_run_epoch\":",
            span.context.source_event_ordinal,
            span.context.epochs.clock,
            span.context.epochs.configuration
        )?;
        source_trace::optional(output, span.context.byte_run_epoch)?;
        // Per-span owner charge is not a sum: aliases can share the same owner.
        write!(
            output,
            ",\"owner_retained_capacity\":{},\"owner_storage_bytes\":{}}}",
            span.backing_bytes().retained_capacity(),
            span.backing_bytes().retained_storage_bytes()
        )?;
    }
    // Public MPEGTS stream-ID side-data observation for this admitted profile.
    write!(
        output,
        "],\"native_side_data\":[{{\"type\":19,\"hex\":\"{:02x}\"}}],\"native_time\":",
        packet.native_stream_id
    )?;
    match time {
        Ok(view) => time_json(output, view)?,
        Err(error) => {
            output.write_all(b"{\"error\":")?;
            source_trace::json_string(output, &error.to_string())?;
            output.write_all(b"}")?;
        }
    }
    output.write_all(b",\"emission_cause\":")?;
    cause_json(output, cause)?;
    writeln!(
        output,
        ",\"shared_budget_used\":{},\"shared_budget_peak\":{},\"qualified\":false}}",
        budget.used(),
        budget.peak()
    )
}

#[derive(Clone, Copy)]
struct ScopeRecord {
    scope: NativePesScope,
    audio: Option<NativePrivateAudioCarriage>,
    dvb_namespace: Option<DescriptorNamespace>,
    tsid: u16,
    pmt_pid: u16,
    pcr_pid: u16,
    pmt_version: u8,
    cause: Cause,
}
#[derive(Clone)]
struct Failure {
    error: Error,
    cause: Cause,
    rejection: Option<RejectedTransport>,
}

/// One inline observer, one pending cause, no output queue. All carrier/results
/// retain the same charged input owners as the independent source operation.
struct NativeAdapter {
    budget: Budget,
    program: u16,
    clock: NativePesClockV1,
    observer: Option<NativePesRecoveryObserver>,
    scope: Option<ScopeRecord>,
    pat: Option<(u16, u16)>,
    announce: bool,
    failure: Option<Failure>,
    failed: bool,
    finished: bool,
    packets: u64,
}
impl NativeAdapter {
    fn new(budget: Budget, program: u16) -> triergon_core::Result<Self> {
        Ok(Self {
            budget,
            program,
            clock: NativePesClockV1::new(program)?,
            observer: None,
            scope: None,
            pat: None,
            announce: false,
            failure: None,
            failed: false,
            finished: false,
            packets: 0,
        })
    }
    fn fail(&mut self, error: Error, cause: Cause) {
        if !self.failed {
            self.failure = Some(Failure {
                error,
                cause,
                rejection: self
                    .observer
                    .as_ref()
                    .and_then(|o| o.pending_rejection())
                    .cloned(),
            });
            self.failed = true;
            // Aborting this optional operation releases its pending charged
            // inputs; independent source parsing/diagnostics continue unchanged.
            self.observer = None;
        }
    }
    fn observe(&mut self, event: &TraceEvent, cause: Cause, quarantined: bool) {
        self.observe_inner(event, cause, quarantined);
        if let (Some(failure), TraceEvent::Diagnostic(fault)) = (self.failure.as_mut(), event) {
            if failure.rejection.is_none() {
                failure.rejection = fault.rejected.clone();
            }
        }
    }
    fn observe_inner(&mut self, event: &TraceEvent, cause: Cause, quarantined: bool) {
        if self.failed {
            return;
        }
        if let Some(mut record) = self.scope {
            if let TraceEvent::Timeline {
                program,
                clock_epoch,
            } = event
            {
                let result = if record.scope.entity == cause.source
                    && *program == record.scope.program
                    && *clock_epoch == cause.epochs.clock
                {
                    self.observer
                        .as_mut()
                        .ok_or(Error::Invalid("native observer missing"))
                        .and_then(|o| {
                            o.commit_source_timeline_v1(NativeSourceContext {
                                source_event_ordinal: cause
                                    .ordinal
                                    .ok_or(Error::Invalid("native Timeline ordinal missing"))?,
                                epochs: cause.epochs,
                                byte_run_epoch: None,
                            })
                        })
                } else {
                    Err(Error::Unsupported("native Timeline source mismatch"))
                };
                match result {
                    Ok(()) => {
                        record.scope.epochs = cause.epochs;
                        self.scope = Some(record);
                    }
                    Err(error) => self.fail(error, cause),
                }
                return;
            }
            if record.scope.entity != cause.source || record.scope.epochs != cause.epochs {
                self.fail(
                    Error::Unsupported("native source entity/epoch transition unobserved"),
                    cause,
                );
                return;
            }
            if quarantined {
                self.fail(Error::Unsupported("native source clock quarantined"), cause);
                return;
            }
        }
        match event {
            TraceEvent::Pat(table) => {
                let mut selected = None;
                let mut programs = 0;
                for entry in table.programs.as_slice() {
                    if !entry.is_network_pid() {
                        programs += 1;
                        if entry.program_number == self.program {
                            selected = Some(entry.pid);
                        }
                    }
                }
                let Some(pid) = selected.filter(|_| programs == 1) else {
                    self.fail(
                        Error::Unsupported("native single selected PAT program not established"),
                        cause,
                    );
                    return;
                };
                let next = (table.header.extension, pid);
                if self.scope.is_some() && self.pat != Some(next) {
                    self.fail(
                        Error::Unsupported("native PAT routing transition unobserved"),
                        cause,
                    );
                } else {
                    self.pat = Some(next);
                }
            }
            TraceEvent::ProgramMap {
                map,
                active: true,
                configuration_epoch,
            } => {
                if self.scope.is_some() {
                    self.fail(
                        Error::Unsupported("native PMT configuration transition unobserved"),
                        cause,
                    );
                    return;
                }
                let Some((tsid, pmt_pid)) = self.pat else {
                    self.fail(
                        Error::Unsupported("native PMT without validated PAT"),
                        cause,
                    );
                    return;
                };
                let streams = map.streams.as_slice();
                if map.header.extension != self.program
                    || streams.len() != 1
                    || !matches!(
                        streams[0].stream_type,
                        0x1b | 0x24 | 0x0f | 0x11 | 6 | 0x81 | 0x87
                    )
                    || *configuration_epoch != cause.epochs.configuration
                    || quarantined
                {
                    self.fail(
                        Error::Unsupported("native single H264 PMT context not established"),
                        cause,
                    );
                    return;
                }
                let carriage = match streams[0].stream_type {
                    0x1b => Some(NativeRegisteredCarriage::Avc),
                    0x24 => Some(NativeRegisteredCarriage::Hevc),
                    0x0f => Some(NativeRegisteredCarriage::AacAdts),
                    0x11 => Some(NativeRegisteredCarriage::AacLatm),
                    _ => None,
                };
                let descriptors = NativePrivateAudioDescriptors {
                    program: map.program_descriptors.as_ref(),
                    elementary: streams[0].descriptor_bytes.as_ref(),
                };
                let dvb_namespace = if streams[0].stream_type == 6
                    && matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
                    && Descriptors::new(descriptors.elementary)
                        .any(|entry| entry.is_ok_and(|view| view.tag == 0x59))
                {
                    Some(map.descriptor_namespace)
                } else {
                    None
                };
                let audio = if carriage.is_none() && dvb_namespace.is_none() {
                    // Unspecified namespaces and unrelated private contexts stay nonpasses.
                    // Neither a source namespace nor a codec frame is inferred.
                    if streams[0].stream_type == 6
                        && !Descriptors::new(descriptors.elementary)
                            .any(|item| item.is_ok_and(|view| matches!(view.tag, 0x6a | 0x7a)))
                    {
                        self.fail(
                            Error::Unsupported("native single H264 PMT context not established"),
                            cause,
                        );
                        return;
                    }
                    match NativePrivateAudioCarriage::from_observed_pmt(
                        streams[0].stream_type,
                        descriptors,
                        ParseLimits::default(),
                    ) {
                        Ok(view) => Some(view),
                        Err(error) => {
                            self.fail(error, cause);
                            return;
                        }
                    }
                } else {
                    None
                };
                let scope = NativePesScope {
                    program: self.program,
                    pid: streams[0].pid,
                    stream_type: streams[0].stream_type,
                    stream_id: if audio.is_some() || dvb_namespace.is_some() {
                        0xbd
                    } else if matches!(
                        carriage,
                        Some(NativeRegisteredCarriage::AacAdts | NativeRegisteredCarriage::AacLatm)
                    ) {
                        0xc0
                    } else {
                        0xe0
                    },
                    entity: cause.source,
                    epochs: cause.epochs,
                };
                let admitted = match (carriage, audio, dvb_namespace) {
                    (Some(NativeRegisteredCarriage::Avc), None, None) => {
                        NativePesRecoveryObserver::new_selected_avc_faults_v1(
                            PROFILE,
                            scope,
                            self.budget.clone(),
                            ParseLimits::default(),
                        )
                    }
                    (Some(carriage), None, None) => NativePesRecoveryObserver::new(
                        PROFILE,
                        scope,
                        carriage,
                        NativeRejectedPolicy::OpcrWithoutPcrV1,
                        self.budget.clone(),
                        ParseLimits::default(),
                    ),
                    (None, Some(audio), None) => NativePesRecoveryObserver::new_private_audio_v1(
                        PROFILE,
                        scope,
                        audio,
                        descriptors,
                        self.budget.clone(),
                        ParseLimits::default(),
                    ),
                    (None, None, Some(namespace)) => {
                        NativePesRecoveryObserver::new_dvb_subtitles_v1(
                            PROFILE,
                            scope,
                            namespace,
                            NativeDvbSubtitlesDescriptors {
                                program: descriptors.program,
                                elementary: descriptors.elementary,
                            },
                            self.budget.clone(),
                            ParseLimits::default(),
                        )
                    }
                    _ => Err(Error::Invalid("native carriage selection")),
                };
                match admitted {
                    Ok(observer) => {
                        self.observer = Some(observer);
                        self.scope = Some(ScopeRecord {
                            scope,
                            audio,
                            dvb_namespace,
                            tsid,
                            pmt_pid,
                            pcr_pid: map.pcr_pid,
                            pmt_version: map.header.version,
                            cause,
                        });
                        self.announce = true;
                    }
                    Err(error) => self.fail(error, cause),
                }
            }
            TraceEvent::Transport(packet) => {
                let Some(record) = self.scope else {
                    if packet.pid != 0
                        && packet.pid != 0x1fff
                        && self.pat.is_none_or(|(_, pid)| pid != packet.pid)
                    {
                        self.fail(
                            Error::Unsupported("native unrouted transport before validated scope"),
                            cause,
                        );
                    }
                    return;
                };
                if packet.pid == record.scope.pid {
                    let Some(ordinal) = cause.ordinal else {
                        self.fail(
                            Error::Invalid("native carrier has no source event ordinal"),
                            cause,
                        );
                        return;
                    };
                    let context = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: cause.epochs,
                        byte_run_epoch: None,
                    };
                    let result = self
                        .observer
                        .as_mut()
                        .ok_or(Error::Invalid("native observer missing"))
                        .and_then(|observer| {
                            observer
                                .push_accepted(packet.clone(), context)
                                .map_err(|rejected| rejected.error)
                        });
                    if let Err(error) = result {
                        self.fail(error, cause);
                    }
                } else if packet.pid == record.pcr_pid && packet.discontinuity_indicator() {
                    self.fail(
                        Error::Unsupported("native selected PCR clock boundary unobserved"),
                        cause,
                    );
                } else if packet.pid == record.pcr_pid && packet.payload.is_some() {
                    self.fail(
                        Error::Unsupported("native unrouted PCR-PID payload context unobserved"),
                        cause,
                    );
                } else if packet.pid != 0
                    && packet.pid != 0x1fff
                    && packet.pid != record.pmt_pid
                    && packet.pid != record.pcr_pid
                {
                    self.fail(
                        Error::Unsupported("native additional unrouted transport unobserved"),
                        cause,
                    );
                }
            }
            TraceEvent::Timeline { .. } | TraceEvent::Discontinuity { .. } => {
                self.fail(
                    Error::Unsupported("native explicit clock/format boundary unobserved"),
                    cause,
                );
            }
            TraceEvent::Diagnostic(fault)
                if fault.kind == FaultKind::Transport
                    || (fault.kind == FaultKind::Pes
                        && matches!(
                            fault.error,
                            Error::Invalid("PES timestamp markers" | "forbidden PES time flags")
                        )) =>
            {
                let result = if let (Some(ordinal), Some(observer)) =
                    (cause.ordinal, self.observer.as_mut())
                {
                    let context = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: cause.epochs,
                        byte_run_epoch: None,
                    };
                    if fault.error == Error::Invalid("PCR reserved bits/extension")
                        && fault.rejected.is_some()
                    {
                        observer
                            .push_diagnostic_rejection_v1(fault.clone(), context)
                            .map_err(|e| e.error)
                    } else if let Some(rejection) = fault.rejected.as_ref() {
                        if rejection.error() != fault.error
                            || rejection.source_id() != Some(fault.location.source)
                            || rejection.source_offset() != fault.location.offset
                        {
                            Err(Error::Invalid(
                                "rejected carrier/source diagnostic mismatch",
                            ))
                        } else {
                            observer
                                .push_rejected(rejection.clone(), context)
                                .map_err(|e| e.error)
                        }
                    } else if fault.pid == Some(observer.scope().pid)
                        && matches!(
                            fault.error,
                            Error::Invalid(
                                "TS transport error indicator"
                                    | "PES timestamp markers"
                                    | "forbidden PES time flags"
                            ) | Error::Unsupported("scrambled TS payload")
                        )
                    {
                        observer.confirm_source_fault_v1(fault, context)
                    } else {
                        Err(Error::Unsupported(
                            "native raw transport unavailable after source rejection",
                        ))
                    }
                } else {
                    Err(Error::Unsupported(
                        "native diagnostic without selected observer/ordinal",
                    ))
                };
                if let Err(error) = result {
                    self.fail(error, cause);
                }
            }
            TraceEvent::Diagnostic(fault)
                if fault.kind == FaultKind::Program
                    || (fault.kind == FaultKind::Psi && self.scope.is_some()) =>
            {
                self.fail(
                    Error::Unsupported("native program/active PSI recovery context unobserved"),
                    cause,
                );
            }
            // Source loss/duplicate/truncation diagnostics remain authoritative;
            // the distinct observer consumes the real carriers, never these PES.
            _ => {}
        }
    }
    fn pending_json(&mut self, output: &mut impl Write) -> io::Result<()> {
        if self.announce {
            if let Some(record) = self.scope {
                let scope = record.scope;
                write!(
                    output,
                    "{{\"kind\":\"native_pes_scope\",\"program\":{},\"pid\":{},\"stream_type\":{},\"stream_id\":{},\"source\":{},\"clock_epoch\":{},\"configuration_epoch\":{},\"tsid\":{},\"pmt_pid\":{},\"pcr_pid\":{},\"pmt_version\":{},\"source_cause\":",
                    scope.program,
                    scope.pid,
                    scope.stream_type,
                    scope.stream_id,
                    scope.entity.0,
                    scope.epochs.clock,
                    scope.epochs.configuration,
                    record.tsid,
                    record.pmt_pid,
                    record.pcr_pid,
                    record.pmt_version
                )?;
                cause_json(output, record.cause)?;
                if let Some(audio) = record.audio {
                    output.write_all(b",\"carriage_view\":")?;
                    source_trace::json_string(output, audio.name())?;
                    output.write_all(b",\"source_namespace_inferred\":false,\"scope_rejected_policy\":\"NoRejectedCarrierV1\"")?;
                }
                if let Some(namespace) = record.dvb_namespace {
                    write!(
                        output,
                        ",\"carriage_view\":\"NativeD32DvbSignalledFinitePesV1\",\"source_namespace_inferred\":false,\"source_descriptor_namespace\":\"{namespace:?}\",\"scope_rejected_policy\":\"NoRejectedCarrierV1\""
                    )?;
                }
                if scope.stream_type == 0x1b {
                    output.write_all(b",\"fault_view\":\"NativeD32SelectedAvcDiagnosticsV1\"")?;
                }
                output.write_all(b",\"qualified\":false}\n")?;
            }
            self.announce = false;
        }
        if let Some(failure) = self.failure.take() {
            output.write_all(b"{\"kind\":\"native_pes_nonpass\",\"operation\":\"ArchivedNativePesPacketsV1\",\"error\":")?;
            source_trace::json_string(output, &failure.error.to_string())?;
            output.write_all(b",\"cause\":")?;
            cause_json(output, failure.cause)?;
            if let Some(rejection) = failure.rejection.as_ref() {
                output.write_all(b",\"rejected_carrier\":")?;
                rejection_json(output, rejection)?;
            }
            output.write_all(b",\"qualified\":false}\n")?;
        }
        Ok(())
    }
    fn drain(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        self.pending_json(output)?;
        if self.failed || self.observer.is_none() {
            return Ok(());
        }
        loop {
            let step = self
                .observer
                .as_mut()
                .expect("checked native observer")
                .poll(&mut WorkBudget::new(4096), cancel);
            match step {
                Ok(AssemblyStep::Output(NativePesObservation::Accepted(packet))) => {
                    let time = self
                        .clock
                        .project_native_header(
                            packet.pid,
                            packet.source_epochs,
                            packet.time_header(),
                            &mut WorkBudget::new(1),
                            cancel,
                        )
                        .and_then(|view| {
                            view.ok_or(Error::Invalid("native clock fixed grant yielded"))
                        });
                    packet_json(output, &packet, self.packets, cause, &time, &self.budget)?;
                    // Drop the owned packet before polling/pushing again.
                    drop(packet);
                    let Some(next) = self.packets.checked_add(1) else {
                        self.fail(Error::Overflow, cause);
                        break;
                    };
                    self.packets = next;
                    if let Err(error) = time {
                        self.fail(error, cause);
                        break;
                    }
                }
                Ok(AssemblyStep::Output(NativePesObservation::FromRejectedCarrier(packet))) => {
                    let time = self
                        .clock
                        .project_encoded_header(
                            packet.pid,
                            &packet.encoded_header,
                            &mut WorkBudget::new(1),
                            cancel,
                        )
                        .and_then(|value| {
                            value.ok_or(Error::Invalid("native encoded clock fixed grant yielded"))
                        });
                    recovered_json(output, &packet, self.packets, cause, &time, &self.budget)?;
                    drop(packet);
                    self.packets = self
                        .packets
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("native packet ordinal overflow"))?;
                    if let Err(error) = time {
                        self.fail(error, cause);
                        break;
                    }
                }
                Ok(AssemblyStep::Output(NativePesObservation::FromSelectedDiagnostic(packet))) => {
                    let time = self
                        .clock
                        .project_diagnostic_header_v1(
                            packet.pid,
                            &packet.encoded_header,
                            &mut WorkBudget::new(1),
                            cancel,
                        )
                        .and_then(|v| {
                            v.ok_or(Error::Invalid(
                                "native diagnostic clock fixed grant yielded",
                            ))
                        });
                    diagnostic_packet_json(
                        output,
                        &packet,
                        self.packets,
                        cause,
                        &time,
                        &self.budget,
                    )?;
                    drop(packet);
                    self.packets = self
                        .packets
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("native packet ordinal overflow"))?;
                    if let Err(error) = time {
                        self.fail(error, cause);
                        break;
                    }
                }
                Ok(AssemblyStep::Yield) => {}
                Ok(AssemblyStep::NeedPayload) => break,
                Ok(AssemblyStep::Finished) => {
                    self.finished = true;
                    break;
                }
                Err(error) => {
                    self.fail(error, cause);
                    break;
                }
            }
        }
        self.pending_json(output)
    }
    fn logical_end(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        if !self.failed {
            let result = self
                .observer
                .as_mut()
                .ok_or(Error::Unsupported(
                    "native scope not established at logical EOF",
                ))
                .and_then(NativePesRecoveryObserver::end_of_source);
            if let Err(error) = result {
                self.fail(error, cause);
            }
        }
        self.drain(output, cause, cancel)?;
        if !self.failed && !self.finished {
            self.fail(Error::Invalid("native logical EOF did not finish"), cause);
            self.pending_json(output)?;
        }
        Ok(())
    }
    fn complete_json(
        &mut self,
        output: &mut impl Write,
        source_events: u64,
        source_finished: bool,
    ) -> io::Result<()> {
        self.pending_json(output)?;
        let status = if self.failed {
            "nonpass"
        } else if !self.finished {
            "incomplete"
        } else if self.packets == 0 {
            "empty_observation_not_acceptance"
        } else {
            "observation_complete_not_parity"
        };
        writeln!(
            output,
            "{{\"kind\":\"native_pes_complete\",\"status\":\"{status}\",\"source_finished\":{source_finished},\"native_finished\":{},\"source_events\":{source_events},\"native_packets\":{},\"shared_budget_used\":{},\"shared_budget_peak\":{},\"qualified\":false}}",
            self.finished,
            self.packets,
            self.budget.used(),
            self.budget.peak()
        )
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let format = match arguments.next().as_deref() {
        Some("188") => PacketFormat::Ts188,
        Some("192") => PacketFormat::M2ts192,
        _ => return Err("usage: mpegts_native_pes_trace 188|192 PROGRAM [--dvb] [--native-clock-v1] FILE... [--discontinuity FILE...]".into()),
    };
    let program: u16 = arguments
        .next()
        .ok_or("missing selected program")?
        .parse()?;
    let mut namespace = DescriptorNamespace::Unspecified;
    let mut source_clock_enabled = false;
    let mut path = loop {
        let argument = arguments.next().ok_or("missing source entity")?;
        match argument.as_str() {
            "--dvb" => {
                namespace =
                    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile)
            }
            "--native-clock-v1" => source_clock_enabled = true,
            _ => break argument,
        }
    };
    // Source-PES and real native-packet time contexts have separate histories.
    let mut source_clock = if source_clock_enabled {
        Some(NativePesClockV1::new(program)?)
    } else {
        None
    };
    let budget = Budget::new(64 * 1024 * 1024)?;
    let cancel = AtomicBool::new(false);
    let mut source_id = 1_u64;
    let mut file = File::open(&path)?;
    let mut input = InputCursor::new(
        &budget,
        SourceId(source_id),
        SourceCapabilities {
            length: Some(file.metadata()?.len()),
            random_access: false,
        },
        16 * 1024,
    )?;
    let mut demux = TransportStream::new_with_namespace(
        budget.clone(),
        format,
        ParseLimits::default(),
        program,
        namespace,
    )?;
    let mut native = NativeAdapter::new(budget.clone(), program)?;
    let mut source_events = 0_u64;
    let mut logical_eof = None;
    let mut buffer = [0_u8; 16 * 1024];
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "{{\"kind\":\"native_pes_operation\",\"operation\":\"ArchivedNativePesPacketsV1\",\"reference_operation\":\"{}\",\"profile\":\"{PROFILE:?}\",\"source_pin\":\"{}\",\"worker_sha256\":\"{}\",\"flags\":{},\"correct_ts_overflow\":{},\"find_stream_info\":{},\"packet_buffer_policy\":\"reference_default\",\"native_packet_cap\":204800,\"rejected_policy\":\"OpcrWithoutPcrV1\",\"native_evidence_version\":2,\"program\":{program},\"source_ordinals\":\"zero_based_source_rows_only\",\"qualified\":false}}",
        PROFILE.operation(),
        PROFILE.source_pin(),
        PROFILE.worker_sha256(),
        PROFILE.flags(),
        PROFILE.default_overflow_correction(),
        PROFILE.find_stream_info()
    )?;
    let execution = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let step = match demux.poll(&mut input, &mut WorkBudget::new(4096), &cancel) {
                Ok(step) => step,
                Err(error) => {
                    let cause = Cause::boundary(
                        "source_poll_error",
                        SourceLocation {
                            source: input.source(),
                            offset: input.position(),
                        },
                        demux.source_epochs(),
                    );
                    native.fail(error, cause);
                    return Err(error.into());
                }
            };
            match step {
                Step::Ready(value) => {
                    let cause =
                        Cause::event(&value, source_events, input.source(), demux.source_epochs());
                    let quarantined = demux.clock_is_quarantined();
                    native.observe(&value, cause, quarantined);
                    let finished = source_trace::event(
                        &mut output,
                        value,
                        source_clock.as_mut(),
                        quarantined,
                        &cancel,
                    )?;
                    source_events = source_events
                        .checked_add(1)
                        .ok_or("source ordinal overflow")?;
                    native.drain(&mut output, cause, &cancel)?;
                    if finished {
                        if let Some(cause) = logical_eof {
                            native.logical_end(&mut output, cause, &cancel)?;
                        } else {
                            native.fail(
                                Error::Invalid("source finished without explicit logical EOF"),
                                cause,
                            );
                        }
                        native.complete_json(&mut output, source_events, true)?;
                        break;
                    }
                }
                Step::Yield => {}
                Step::NeedInput(request) => {
                    let count = match file.read(&mut buffer[..request.maximum]) {
                        Ok(count) => count,
                        Err(error) => {
                            let cause = Cause::boundary(
                                "input_transport_failure",
                                SourceLocation {
                                    source: request.source,
                                    offset: request.offset,
                                },
                                demux.source_epochs(),
                            );
                            native.fail(Error::Transport, cause);
                            input.fail(request.source, request.token, SourceFailure::Transport)?;
                            return Err(error.into());
                        }
                    };
                    if count == 0 {
                        input.finish(request.source, request.token)?;
                    } else {
                        input.provide(&InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            Bytes::copy_from_slice(&budget, &buffer[..count])?,
                        )?)?;
                    }
                }
                Step::End(location) => {
                    if let Some(next) = arguments.next() {
                        native.fail(
                            Error::Unsupported("native entity boundary context unobserved"),
                            Cause::boundary(
                                "entity_end_not_logical_eof",
                                location,
                                demux.source_epochs(),
                            ),
                        );
                        native.pending_json(&mut output)?;
                        let discontinuity = next == "--discontinuity";
                        path = if discontinuity {
                            arguments
                                .next()
                                .ok_or("missing entity after discontinuity")?
                        } else {
                            next
                        };
                        if discontinuity {
                            demux.discontinuity()?;
                        }
                        file = File::open(&path)?;
                        source_id = source_id.checked_add(1).ok_or("source id overflow")?;
                        input.continue_source(
                            SourceId(source_id),
                            SourceCapabilities {
                                length: Some(file.metadata()?.len()),
                                random_access: false,
                            },
                        )?;
                    } else {
                        logical_eof = Some(Cause::boundary(
                            "logical_eof",
                            location,
                            demux.source_epochs(),
                        ));
                        demux.end_of_source()?;
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = execution {
        let cause = Cause::boundary(
            "composed_driver_error",
            SourceLocation {
                source: input.source(),
                offset: input.position(),
            },
            demux.source_epochs(),
        );
        native.fail(
            error
                .downcast_ref::<Error>()
                .copied()
                .unwrap_or(Error::Invalid(
                    "composed driver failed; see driver error row",
                )),
            cause,
        );
        native.pending_json(&mut output)?;
        output.write_all(b"{\"kind\":\"native_pes_driver_error\",\"error\":")?;
        source_trace::json_string(&mut output, &error.to_string())?;
        output.write_all(b",\"cause\":")?;
        cause_json(&mut output, cause)?;
        output.write_all(b",\"qualified\":false}\n")?;
        native.complete_json(&mut output, source_events, false)?;
        output.flush()?;
        return Err(error);
    }
    output.flush()?;
    if native.failed {
        return Err("native operation has an explicit retained nonpass".into());
    }
    Ok(())
}
