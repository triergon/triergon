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
use triergon_mpegts::native_input::{
    NativeInputBootstrap, NativeInputContext, NativeInputStep, NativePositionBasis,
};
use triergon_mpegts::native_pes::{
    NativeDvbSubtitlesDescriptors, NativePesPacket, NativePesScope, NativePrivateAudioCarriage,
    NativePrivateAudioDescriptors, NativeRegisteredCarriage, NativeSourceContext,
};
use triergon_mpegts::native_recovery::{
    NativeDiagnosticPesPacket, NativePesObservation, NativePesRecoveryObserver,
    NativeRecoveredPesPacket, NativeRejectedPolicy,
};
use triergon_mpegts::native_recovery::{NativePatRoute, NativePatSelection};
use triergon_mpegts::psi::{DescriptorNamespace, Descriptors, DvbNamespaceEvidence};
use triergon_mpegts::{
    AssemblyStep, FaultKind, PacketFormat, ParseLimits, RejectedTransport, SourceEpochs,
    SourceRange, TraceEvent, TransportStream,
};

const PROFILE: NativeClockProfile = NativeClockProfile::ArchivedPesV1;

#[path = "support/native_object_v1.rs"]
mod native_object_v1;

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
    initial_input: Option<NativeInputContext>,
    scope: Option<ScopeRecord>,
    pat: Option<(u16, u16)>,
    pat_route: Option<NativePatRoute>,
    scope_update: Option<(ScopeRecord, ScopeRecord)>,
    announce: bool,
    failure: Option<Failure>,
    failed: bool,
    finished: bool,
    packets: u64,
    pending_media_discontinuity: Option<(SourceId, SourceEpochs)>,
    media_entities: bool,
    object: Option<native_object_v1::ObjectAdapter>,
    packet_completion: Option<triergon_mpegts::native_packet_completion::NativePacketCompletionV1>,
}
impl NativeAdapter {
    fn new(budget: Budget, program: u16) -> triergon_core::Result<Self> {
        Ok(Self {
            budget,
            program,
            clock: NativePesClockV1::new(program)?,
            observer: None,
            initial_input: None,
            scope: None,
            pat: None,
            pat_route: None,
            scope_update: None,
            announce: false,
            failure: None,
            failed: false,
            finished: false,
            packets: 0,
            pending_media_discontinuity: None,
            media_entities: false,
            object: None,
            packet_completion: None,
        })
    }
    fn new_object_v1(budget: Budget, program: u16) -> triergon_core::Result<Self> {
        let mut native = Self::new(budget.clone(), program)?;
        native.object = Some(native_object_v1::ObjectAdapter::new(budget, program)?);
        Ok(native)
    }
    fn new_psi_compatibility_v1(
        budget: Budget,
        program: u16,
        policies: triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1,
    ) -> triergon_core::Result<Self> {
        let mut native = Self::new(budget.clone(), program)?;
        native.object = Some(native_object_v1::ObjectAdapter::new_psi_compatibility_v1(
            budget, program, policies,
        )?);
        Ok(native)
    }
    fn new_packet_completion_v1(budget: Budget, program: u16) -> triergon_core::Result<Self> {
        let mut native = Self::new_psi_compatibility_v1(
            budget.clone(),
            program,
            triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1::ALL_OBSERVED,
        )?;
        native.packet_completion = Some(
            triergon_mpegts::native_packet_completion::NativePacketCompletionV1::new(
                budget,
                ParseLimits::default(),
                program,
            )?,
        );
        Ok(native)
    }
    fn new_selected_object_v2(
        budget: Budget,
        program: u16,
        format: PacketFormat,
    ) -> triergon_core::Result<Self> {
        let mut native = Self::new(budget.clone(), program)?;
        native.object = Some(native_object_v1::ObjectAdapter::new_selected_carriage_v2(
            budget.clone(),
            program,
            format,
        )?);
        native.packet_completion = Some(
            triergon_mpegts::native_packet_completion::NativePacketCompletionV1::new_for_format_v2(
                budget,
                ParseLimits::default(),
                program,
                format,
            )?,
        );
        Ok(native)
    }
    fn object_end_with_cancel_v1(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &InputCursor,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        let object = self
            .object
            .as_mut()
            .ok_or(Error::Invalid("native whole object not enabled"))?;
        if object.uses_psi_compatibility_v1() {
            object.end_psi_with_cancel_v1(end, input, cancel)
        } else {
            object.end(end, input)
        }
    }
    fn packet_physical_end_v1(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        let result = self.object_end_with_cancel_v1(end, input, cancel);
        let sealed = (|| {
            let completion = self
                .packet_completion
                .as_mut()
                .ok_or(Error::Invalid("packet completion profile not enabled"))?;
            let cost = completion.required_end_work_v1()?;
            if !completion.seal_physical_end_v1(end, input, &mut WorkBudget::new(cost), cancel)? {
                return Err(Error::Invalid("packet completion exact End grant yielded"));
            }
            Ok(())
        })();
        if let Err(error) = sealed {
            self.fail(
                error,
                Cause::boundary(
                    "packet_completion_physical_end",
                    end.location(),
                    end.epochs(),
                ),
            );
            return Err(error);
        }
        result
    }
    #[cfg(test)]
    fn object_end_v1(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &InputCursor,
    ) -> triergon_core::Result<()> {
        self.object
            .as_mut()
            .ok_or(Error::Invalid("native whole object not enabled"))?
            .end(end, input)
    }
    #[cfg(test)]
    fn object_json(&self, output: &mut impl Write) -> io::Result<bool> {
        if self.packet_completion.is_some() && (self.failed || !self.finished) {
            return Ok(false);
        }
        let complete = match self.object.as_ref() {
            Some(object) => object.json(output, self.failed, self.finished, self.packets)?,
            None => false,
        };
        if complete {
            output.write_all(b"\n")?;
        }
        Ok(complete)
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
            self.initial_input = None;
        }
    }
    fn media_entity_boundary_v1(
        &mut self,
        end: &triergon_mpegts::MediaEntityEndV1,
        input: &mut InputCursor,
        discontinuity: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<bool> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(false);
        }
        if self.failed || self.finished || self.pending_media_discontinuity.is_some() {
            return Err(Error::Invalid("native media operation not available"));
        }
        end.validate_continuation(input)?;
        let record = self.scope.ok_or(Error::Unsupported(
            "native media boundary before validated scope",
        ))?;
        if record.scope.entity != end.location().source || record.scope.epochs != end.epochs() {
            return Err(Error::Invalid(
                "native media boundary source scope mismatch",
            ));
        }
        if discontinuity {
            let epochs = SourceEpochs {
                clock: end.epochs().clock.checked_add(1).ok_or(Error::Overflow)?,
                configuration: end
                    .epochs()
                    .configuration
                    .checked_add(1)
                    .ok_or(Error::Overflow)?,
            };
            let clock = NativePesClockV1::new(self.program)?;
            input.read_into(&mut [], work, cancel)?;
            if !work.try_charge(1) {
                return Ok(false);
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            self.observer = None;
            self.initial_input = None;
            self.scope = None;
            self.pat = None;
            self.pat_route = None;
            self.scope_update = None;
            self.announce = false;
            self.clock = clock;
            self.pending_media_discontinuity = Some((input.source(), epochs));
        } else {
            let observer = self
                .observer
                .as_mut()
                .ok_or(Error::Invalid("native media observer missing"))?;
            if !observer.continue_media_entity_v1(end, input, work, cancel)? {
                return Ok(false);
            }
            let mut next = record;
            next.scope = observer.scope();
            self.scope = Some(next);
        }
        self.media_entities = true;
        Ok(true)
    }
    #[cfg(test)]
    fn observe(&mut self, event: &TraceEvent, cause: Cause, quarantined: bool) {
        self.observe_with_cancel(event, cause, quarantined, &AtomicBool::new(false));
    }
    fn observe_with_cancel(
        &mut self,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
        cancel: &AtomicBool,
    ) {
        self.observe_inner(event, cause, quarantined, cancel, None);
        if let (Some(failure), TraceEvent::Diagnostic(fault)) = (self.failure.as_mut(), event) {
            if failure.rejection.is_none() {
                failure.rejection = fault.rejected.clone();
            }
        }
    }
    fn observe_with_input(
        &mut self,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
        cancel: &AtomicBool,
        input: &InputCursor,
    ) {
        if let Some(context) = &self.initial_input {
            if let Err(error) = context.validate_cursor(input) {
                self.fail(error, cause);
                return;
            }
        }
        if let Some(observer) = &self.observer {
            if let Err(error) = observer.validate_native_input_v1(input) {
                self.fail(error, cause);
                return;
            }
        }
        self.observe_inner(event, cause, quarantined, cancel, Some(input));
        if let (Some(failure), TraceEvent::Diagnostic(fault)) = (self.failure.as_mut(), event) {
            if failure.rejection.is_none() {
                failure.rejection = fault.rejected.clone();
            }
        }
    }
    fn observe_inner(
        &mut self,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
        cancel: &AtomicBool,
        input: Option<&InputCursor>,
    ) {
        if self.packet_completion.is_some() {
            let admitted = (|| {
                let context = NativeSourceContext {
                    source_event_ordinal: cause.ordinal.ok_or(Error::Invalid(
                        "packet completion actual source ordinal missing",
                    ))?,
                    epochs: cause.epochs,
                    byte_run_epoch: None,
                };
                let completion = self
                    .packet_completion
                    .as_mut()
                    .ok_or(Error::Invalid("packet completion state missing"))?;
                let cost = completion.required_work_v1(event)?;
                if !completion.observe_v1(
                    event,
                    cause.source,
                    context,
                    &mut WorkBudget::new(cost),
                    cancel,
                )? {
                    return Err(Error::Invalid(
                        "packet completion exact event grant yielded",
                    ));
                }
                Ok(())
            })();
            if let Err(error) = admitted {
                self.fail(error, cause);
                return;
            }
        }
        if let Some(object) = self.object.as_mut() {
            let idle = self
                .observer
                .as_ref()
                .is_none_or(NativePesRecoveryObserver::discard_idle_v1);
            let skipped = if object.uses_psi_compatibility_v1() {
                if let Some(ordinal) = cause.ordinal {
                    object.observe_psi_context_v1(
                        event,
                        idle,
                        cause.source,
                        NativeSourceContext {
                            source_event_ordinal: ordinal,
                            epochs: cause.epochs,
                            byte_run_epoch: None,
                        },
                        cancel,
                    )
                } else {
                    object.missing_psi_source_context_v1();
                    return;
                }
            } else {
                object.observe(event, idle, cancel)
            };
            if skipped {
                return;
            }
        }
        if self.failed {
            return;
        }
        if let Some((source, expected)) = self.pending_media_discontinuity {
            if matches!(event, TraceEvent::Discontinuity { epochs } if *epochs == expected)
                && cause.source == source
                && cause.epochs == expected
            {
                self.pending_media_discontinuity = None;
            } else {
                self.fail(
                    Error::Invalid("native media reset lacks actual source discontinuity"),
                    cause,
                );
            }
            return;
        }
        if let (
            Some(record),
            TraceEvent::ProgramMap {
                map,
                active: true,
                configuration_epoch,
            },
        ) = (self.scope, event)
        {
            if record.scope.stream_type == 0x1b
                && map.streams.as_slice().len() == 1
                && map.streams.as_slice()[0].stream_type == 0x24
            {
                let result = (|| {
                    if quarantined
                        || cause.source != record.scope.entity
                        || *configuration_epoch != cause.epochs.configuration
                    {
                        return Err(Error::Unsupported(
                            "selected PMT source/quarantine context unobserved",
                        ));
                    }
                    if self.scope_update.is_some() {
                        return Err(Error::Invalid("selected PMT scope update not drained"));
                    }
                    let route = self
                        .pat_route
                        .ok_or(Error::Invalid("selected PAT route missing"))?;
                    let ordinal = cause
                        .ordinal
                        .ok_or(Error::Invalid("selected PMT source ordinal missing"))?;
                    let observer = self
                        .observer
                        .as_mut()
                        .ok_or(Error::Invalid("native observer missing"))?;
                    let mut job = observer.rebind_drained_selected_pmt_v1(
                        route,
                        map,
                        NativeSourceContext {
                            source_event_ordinal: ordinal,
                            epochs: cause.epochs,
                            byte_run_epoch: None,
                        },
                    )?;
                    loop {
                        match job.poll(&mut WorkBudget::new(4096), cancel)? {
                            triergon_mpegts::psi::TableStep::Yield => {}
                            triergon_mpegts::psi::TableStep::Output(binding) => {
                                return Ok(binding);
                            }
                            triergon_mpegts::psi::TableStep::Done => {
                                return Err(Error::Invalid("selected PMT binding output missing"));
                            }
                        }
                    }
                })();
                match result {
                    Ok(binding) => {
                        let mut next = record;
                        next.scope = binding.scope;
                        next.pcr_pid = binding.pcr_pid;
                        next.pmt_version = binding.version;
                        next.cause = cause;
                        self.scope_update = Some((record, next));
                        self.scope = Some(next);
                    }
                    Err(error) => self.fail(error, cause),
                }
                return;
            }
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
            // A source PES can complete in a later entity while its real header
            // and causal origin still name the previous one. It is not a new
            // transport admission and must retain that original source identity.
            let historical_pes = self.media_entities && matches!(event, TraceEvent::Pes(_));
            if (!historical_pes && record.scope.entity != cause.source)
                || record.scope.epochs != cause.epochs
            {
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
                // A real PAT containing only network_PID entries has no PES
                // selection to validate. The control-only profile still needs
                // complete physical coverage and actual EOF before success.
                if self.packet_completion.is_some()
                    && self.scope.is_none()
                    && table.programs.as_slice().iter().all(|p| p.is_network_pid())
                {
                    return;
                }
                let chosen = (|| {
                    let ordinal = cause
                        .ordinal
                        .ok_or(Error::Invalid("selected PAT source ordinal missing"))?;
                    let mut search = NativePatSelection::new(
                        table,
                        self.program,
                        &self.budget,
                        NativeSourceContext {
                            source_event_ordinal: ordinal,
                            epochs: cause.epochs,
                            byte_run_epoch: None,
                        },
                    )?;
                    loop {
                        match search.poll(&mut WorkBudget::new(4096), cancel)? {
                            triergon_mpegts::psi::TableStep::Yield => {}
                            triergon_mpegts::psi::TableStep::Output(route) => return Ok(route),
                            triergon_mpegts::psi::TableStep::Done => {
                                return Err(Error::Invalid("selected PAT output missing"));
                            }
                        }
                    }
                })();
                let route = match chosen {
                    Ok(Some(route)) if route.entity() == cause.source => route,
                    Ok(_) => {
                        self.fail(
                            Error::Unsupported(
                                "native single selected PAT program not established",
                            ),
                            cause,
                        );
                        return;
                    }
                    Err(error) => {
                        self.fail(error, cause);
                        return;
                    }
                };
                let pid = route.pmt_pid();
                let next = (table.header.extension, pid);
                if self.scope.is_some() && self.pat != Some(next) {
                    self.fail(
                        Error::Unsupported("native PAT routing transition unobserved"),
                        cause,
                    );
                } else {
                    self.pat = Some(next);
                    self.pat_route = Some(route);
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
                    Ok(mut observer) => {
                        if let Some(context) = &self.initial_input {
                            let binding = input
                                .ok_or(Error::Invalid("native startup input missing"))
                                .and_then(|input| observer.bind_native_input_v1(context, input));
                            if let Err(error) = binding {
                                self.fail(error, cause);
                                return;
                            }
                        }

                        if matches!(scope.stream_type, 0x1b | 0x24) {
                            let bound = (|| {
                                let route = self
                                    .pat_route
                                    .ok_or(Error::Invalid("selected PAT route missing"))?;
                                let ordinal = cause
                                    .ordinal
                                    .ok_or(Error::Invalid("selected PMT source ordinal missing"))?;
                                let mut job = observer.bind_selected_pmt_v1(
                                    route,
                                    map,
                                    NativeSourceContext {
                                        source_event_ordinal: ordinal,
                                        epochs: cause.epochs,
                                        byte_run_epoch: None,
                                    },
                                )?;
                                loop {
                                    match job.poll(&mut WorkBudget::new(4096), cancel)? {
                                        triergon_mpegts::psi::TableStep::Yield => {}
                                        triergon_mpegts::psi::TableStep::Output(_) => {
                                            return Ok(());
                                        }
                                        triergon_mpegts::psi::TableStep::Done => {
                                            return Err(Error::Invalid(
                                                "initial selected PMT output missing",
                                            ));
                                        }
                                    }
                                }
                            })();
                            if let Err(error) = bound {
                                self.fail(error, cause);
                                return;
                            }
                        }
                        // Assembly retained only sealed token/range/basis facts.
                        // Release the startup alias before normal PES processing.
                        self.initial_input = None;
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
                    if let (Some(completion), Some(ordinal)) =
                        (self.packet_completion.as_ref(), cause.ordinal)
                    {
                        if completion.is_control_transport_v1(
                            packet.pid,
                            NativeSourceContext {
                                source_event_ordinal: ordinal,
                                epochs: cause.epochs,
                                byte_run_epoch: None,
                            },
                        ) {
                            return;
                        }
                    }
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
                            if let Some(input) = input {
                                observer
                                    .push_accepted_with_input(packet.clone(), context, input)
                                    .map_err(|rejected| rejected.error)
                            } else {
                                observer
                                    .push_accepted(packet.clone(), context)
                                    .map_err(|rejected| rejected.error)
                            }
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
                if self
                    .observer
                    .as_ref()
                    .is_some_and(|o| o.pmt_binding_v1().is_some())
                {
                    write!(
                        output,
                        ",\"lifecycle_view\":\"NativeSelectedProgramLifecycleV1\",\"selected_pat_programs\":{}",
                        self.pat_route.map_or(0, NativePatRoute::program_count)
                    )?;
                }
                output.write_all(b",\"qualified\":false}\n")?;
            }
            self.announce = false;
        }
        if let Some((old, next)) = self.scope_update.take() {
            write!(
                output,
                "{{\"kind\":\"native_pes_scope_update\",\"operation\":\"ArchivedNativePesPacketsV1\",\"lifecycle_view\":\"NativeSelectedProgramLifecycleV1\",\"source\":{},\"program\":{},\"pid\":{},\"pmt_pid\":{},\"pcr_pid\":{},\"old_stream_type\":{},\"new_stream_type\":{},\"old_pmt_version\":{},\"new_pmt_version\":{},\"old_clock_epoch\":{},\"new_clock_epoch\":{},\"old_configuration_epoch\":{},\"new_configuration_epoch\":{},\"source_cause\":",
                next.scope.entity.0,
                next.scope.program,
                next.scope.pid,
                next.pmt_pid,
                next.pcr_pid,
                old.scope.stream_type,
                next.scope.stream_type,
                old.pmt_version,
                next.pmt_version,
                old.scope.epochs.clock,
                next.scope.epochs.clock,
                old.scope.epochs.configuration,
                next.scope.epochs.configuration
            )?;
            cause_json(output, next.cause)?;
            output.write_all(b",\"qualified\":false}\n")?;
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
    #[cfg(test)]
    fn drain(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        self.drain_with_input(output, cause, cancel, None)
    }
    fn drain_with_input(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
        input: Option<&InputCursor>,
    ) -> io::Result<()> {
        self.pending_json(output)?;
        if self.failed || self.observer.is_none() {
            return Ok(());
        }
        loop {
            let observer = self.observer.as_mut().expect("checked native observer");
            let step = if let Some(input) = input {
                observer.poll_with_input(input, &mut WorkBudget::new(4096), cancel)
            } else {
                observer.poll(&mut WorkBudget::new(4096), cancel)
            };
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
                    if let Some(object) = self.object.as_mut() {
                        match time.as_ref() {
                            Ok(view) => object.capture(packet, view),
                            Err(_) => object.unsupported_packet(),
                        }
                    } else {
                        drop(packet);
                    }
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
                    if let Some(object) = self.object.as_mut() {
                        object.unsupported_packet();
                    }
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
                    if let Some(object) = self.object.as_mut() {
                        object.unsupported_packet();
                    }
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
    #[cfg(test)]
    fn logical_end(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        self.logical_end_with_input(output, cause, cancel, None)
    }
    fn logical_end_with_input(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
        input: Option<&InputCursor>,
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
        self.drain_with_input(output, cause, cancel, input)?;
        if !self.failed && !self.finished {
            self.fail(Error::Invalid("native logical EOF did not finish"), cause);
            self.pending_json(output)?;
        }
        Ok(())
    }
    fn packet_logical_end_v1(
        &mut self,
        output: &mut impl Write,
        cause: Cause,
        cancel: &AtomicBool,
        input: &mut InputCursor,
    ) -> io::Result<()> {
        if self.failed {
            return self.pending_json(output);
        }
        let result = (|| {
            let completion = self
                .packet_completion
                .as_mut()
                .ok_or(Error::Invalid("packet completion profile not enabled"))?;
            if !completion.validate_finished_v1(input, &mut WorkBudget::new(1), cancel)? {
                return Err(Error::Invalid(
                    "packet completion exact terminal grant yielded",
                ));
            }
            self.object
                .as_ref()
                .ok_or(Error::Invalid("packet completion whole object missing"))?
                .packet_completion_ready_v1()?;
            if self.observer.is_none() {
                if self.scope.is_some() || self.packets != 0 {
                    return Err(Error::Invalid(
                        "packet completion no-media/observer state mismatch",
                    ));
                }
                let cost = completion.required_end_work_v1()?;
                if completion
                    .confirm_no_media_v1(input, &mut WorkBudget::new(cost), cancel)?
                    .is_none()
                {
                    return Err(Error::Invalid(
                        "packet completion exact no-media grant yielded",
                    ));
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(error, cause);
            return self.pending_json(output);
        }
        if self.observer.is_none() {
            // The typed proof contains no PES scope or clock. Zero packets are
            // derived from exhaustive physical non-media carriers and real EOF.
            self.finished = true;
            self.pending_json(output)
        } else {
            self.logical_end_with_input(output, cause, cancel, Some(input))
        }
    }
    fn packet_completion_json_v1(&self, output: &mut impl Write) -> io::Result<()> {
        let Some(completion) = self.packet_completion.as_ref() else {
            return Ok(());
        };
        write!(
            output,
            "{{\"kind\":\"native_packet_completion\",\"operation\":\"NativeD32PhysicalPacketCompletionV1\",\"completed\":{},\"actual_validated_scope\":{},\"native_packets\":{},\"control_only_receipt\":",
            !self.failed && self.finished,
            self.scope.is_some(),
            self.packets
        )?;
        if let Some(receipt) = completion.receipt() {
            write!(
                output,
                "{{\"source\":{},\"source_bytes\":{},\"physical_carriers\":{},\"pat_facts\":{},\"pmt_sections\":{},\"source_finished_ordinal\":{}}}",
                receipt.source().0,
                receipt.source_bytes(),
                receipt.physical_carriers(),
                receipt.pat_facts(),
                receipt.pmt_sections(),
                receipt.source_finished().source_event_ordinal
            )?;
        } else {
            output.write_all(b"null")?;
        }
        output.write_all(b",\"actual_carriers\":[")?;
        for (index, evidence) in completion.physical_carriers().iter().enumerate() {
            if index != 0 {
                output.write_all(b",")?;
            }
            let p = evidence.packet();
            let c = evidence.context();
            write!(
                output,
                "{{\"source\":{},\"offset\":{},\"pid\":{},\"source_event_ordinal\":{},\"source_clock_epoch\":{},\"source_configuration_epoch\":{},\"classification\":",
                p.source_id
                    .ok_or_else(|| io::Error::other("packet completion source missing"))?
                    .0,
                p.source_offset,
                p.pid,
                c.source_event_ordinal,
                c.epochs.clock,
                c.epochs.configuration
            )?;
            source_trace::json_string(output, &format!("{:?}", evidence.kind()))?;
            output.write_all(b",\"raw_hex\":")?;
            source_trace::hex(output, p.source.as_ref())?;
            output.write_all(b"}")?;
        }
        output.write_all(b"],\"raw_pat_associations\":[")?;
        for (index, evidence) in completion.raw_pat_facts().iter().enumerate() {
            if index != 0 {
                output.write_all(b",")?;
            }
            let section = evidence.section();
            let h = section.header;
            write!(
                output,
                "{{\"source_event_ordinal\":{},\"current_next\":{},\"section_number\":{},\"last_section_number\":{},\"version\":{},\"raw_hex\":",
                evidence.context().source_event_ordinal,
                h.current_next,
                h.section_number,
                h.last_section_number,
                h.version
            )?;
            source_trace::hex(output, section.source.as_ref())?;
            output.write_all(b"}")?;
        }
        output.write_all(b"],\"no_synthetic_scope_or_clock\":true,\"qualified\":false}\n")
    }
    fn complete_json(
        &mut self,
        output: &mut impl Write,
        source_events: u64,
        source_finished: bool,
    ) -> io::Result<()> {
        self.pending_json(output)?;
        let status = if !self.failed && self.finished && self.packet_completion.is_some() {
            "packet_view_complete_not_qualification"
        } else if self.failed {
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

#[derive(Default)]
struct NativeStartupV1 {
    eof: bool,
    failure: bool,
    byte: Option<u8>,
    result: Option<triergon_core::Result<NativeInputContext>>,
}

fn prepare_native_startup_v1(
    reader: &mut impl Read,
    input: &mut InputCursor,
    budget: &Budget,
    format: PacketFormat,
    enabled: bool,
    buffer: &mut [u8; 16 * 1024],
    cancel: &AtomicBool,
) -> Result<NativeStartupV1, Box<dyn std::error::Error>> {
    let mut startup = NativeStartupV1::default();
    // The opt-in finite operation uses one real InputWindow, not a second
    // parser/cursor. A positive-size terminal read is kept separate from the
    // advertised size and deferred core finish/fail until the prefix drains.
    if enabled && format == PacketFormat::M2ts192 {
        let request = input.request();
        if request.maximum != 0 {
            let count = reader.read(&mut buffer[..request.maximum])?;
            let window = InputWindow::new(
                request.source,
                request.token.clone(),
                0,
                Bytes::copy_from_slice(budget, &buffer[..count])?,
            )?;
            input.provide(&window)?;
            let mut terminal = [0_u8; 1];
            startup.result = Some(match reader.read(&mut terminal) {
                Ok(0) => {
                    startup.eof = true;
                    (|| {
                        let mut job = NativeInputBootstrap::new(input, &window, format)?;
                        job.observe_entity_eof(
                            input,
                            request.source,
                            &request.token,
                            count as u64,
                        )?;
                        loop {
                            match job.poll(input, &mut WorkBudget::new(4096), cancel)? {
                                NativeInputStep::Ready(context) => return Ok(context),
                                NativeInputStep::Yield => {}
                                NativeInputStep::NeedEntityEof => {
                                    return Err(Error::Invalid("real startup EOF lost"));
                                }
                            }
                        }
                    })()
                }
                Ok(_) => {
                    startup.byte = Some(terminal[0]);
                    Err(Error::Unsupported(
                        "native startup finite-window evidence incomplete",
                    ))
                }
                Err(_) => {
                    startup.failure = true;
                    Err(Error::Transport)
                }
            });
        } else {
            startup.result = Some(Err(Error::Unsupported("native startup empty entity")));
        }
    }
    Ok(startup)
}

fn apply_native_startup_v1(
    native: &mut NativeAdapter,
    startup: &mut NativeStartupV1,
    input: &InputCursor,
    epochs: SourceEpochs,
) {
    if let Some(result) = startup.result.take() {
        match result {
            Ok(context) => native.initial_input = Some(context),
            Err(error) => native.fail(
                error,
                Cause::boundary(
                    "native_startup_context",
                    SourceLocation {
                        source: input.source(),
                        offset: 0,
                    },
                    epochs,
                ),
            ),
        }
    }
}

fn native_input_operation_json(
    output: &mut impl Write,
    native: &NativeAdapter,
    input: &InputCursor,
    startup_eof: bool,
) -> io::Result<()> {
    let basis = match native.initial_input.as_ref().map(NativeInputContext::basis) {
        Some(NativePositionBasis::CarrierStart) => "carrier_start",
        Some(NativePositionBasis::SyncStart) => "sync_start",
        None => "unavailable",
    };
    writeln!(
        output,
        "{{\"kind\":\"native_input_operation\",\"operation\":\"FiniteAlignedNativeInputV1\",\"real_initial_eof\":{startup_eof},\"context_available\":{},\"basis\":\"{basis}\",\"inspected_bytes\":{},\"source\":{},\"input_generation\":{},\"input_request\":{},\"native_callback_maximum\":65536,\"native_avio_buffer\":32768,\"native_reference_worker_sha256\":\"7f9372f35a5a6b26494e5c41a8579bd0654c1142733058738948078b856cdd67\",\"qualified\":false}}",
        native.initial_input.is_some(),
        native
            .initial_input
            .as_ref()
            .map_or(0, NativeInputContext::inspected_bytes),
        input.source().0,
        input.token().generation(),
        input.token().request()
    )?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let format = match arguments.next().as_deref() {
        Some("188") => PacketFormat::Ts188,
        Some("192") => PacketFormat::M2ts192,
        _ => return Err("usage: mpegts_native_pes_trace 188|192 PROGRAM [--dvb] [--native-clock-v1] [--media-entities-v1] [--native-object-v1|--native-psi-compatibility-v1] FILE... [--discontinuity FILE...]".into()),
    };
    let program: u16 = arguments
        .next()
        .ok_or("missing selected program")?
        .parse()?;
    let mut namespace = DescriptorNamespace::Unspecified;
    let mut source_clock_enabled = false;
    let mut native_input_enabled = false;
    let mut media_entities_enabled = false;
    let mut whole_object_enabled = false;
    let mut psi_compatibility_enabled = false;
    let mut packet_completion_enabled = false;
    // Explicit selected audio/M2TS whole-object operation; old flags stay literal.
    let mut selected_object_v2 = false;
    let mut path = loop {
        let argument = arguments.next().ok_or("missing source entity")?;
        match argument.as_str() {
            "--dvb" => {
                namespace =
                    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile)
            }
            "--native-clock-v1" => source_clock_enabled = true,
            "--native-input-v1" => native_input_enabled = true,
            "--media-entities-v1" => media_entities_enabled = true,
            "--native-object-v1" => whole_object_enabled = true,
            "--native-selected-object-v2" => {
                selected_object_v2 = true;
                whole_object_enabled = true;
                psi_compatibility_enabled = true;
                packet_completion_enabled = true;
                native_input_enabled = format == PacketFormat::M2ts192;
            }
            "--native-packet-completion-v1" => {
                whole_object_enabled = true;
                psi_compatibility_enabled = true;
                packet_completion_enabled = true;
            }
            "--native-psi-compatibility-v1" => {
                whole_object_enabled = true;
                psi_compatibility_enabled = true;
            }
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
    if whole_object_enabled
        && (media_entities_enabled
            || namespace != DescriptorNamespace::Unspecified
            || (!selected_object_v2 && (format != PacketFormat::Ts188 || native_input_enabled)))
    {
        return Err(
            "native whole object is the explicit single-entity TS188 video operation".into(),
        );
    }
    let mut native = if selected_object_v2 {
        NativeAdapter::new_selected_object_v2(budget.clone(), program, format)?
    } else if packet_completion_enabled {
        NativeAdapter::new_packet_completion_v1(budget.clone(), program)?
    } else if psi_compatibility_enabled {
        NativeAdapter::new_psi_compatibility_v1(
            budget.clone(),
            program,
            triergon_mpegts::native_catalogue::NativePsiCompatibilityPoliciesV1::ALL_OBSERVED,
        )?
    } else if whole_object_enabled {
        NativeAdapter::new_object_v1(budget.clone(), program)?
    } else {
        NativeAdapter::new(budget.clone(), program)?
    };
    let mut source_events = 0_u64;
    let mut logical_eof = None;
    let mut buffer = [0_u8; 16 * 1024];
    let mut startup = prepare_native_startup_v1(
        &mut file,
        &mut input,
        &budget,
        format,
        native_input_enabled,
        &mut buffer,
        &cancel,
    )?;
    apply_native_startup_v1(&mut native, &mut startup, &input, demux.source_epochs());

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
    if native_input_enabled {
        native_input_operation_json(&mut output, &native, &input, startup.eof)?;
    }
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
                    if native_input_enabled {
                        native.observe_with_input(&value, cause, quarantined, &cancel, &input);
                    } else {
                        native.observe_with_cancel(&value, cause, quarantined, &cancel);
                    }
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
                    native.drain_with_input(
                        &mut output,
                        cause,
                        &cancel,
                        native_input_enabled.then_some(&input),
                    )?;
                    if finished {
                        if let Some(cause) = logical_eof {
                            if packet_completion_enabled {
                                native.packet_logical_end_v1(
                                    &mut output,
                                    cause,
                                    &cancel,
                                    &mut input,
                                )?;
                            } else {
                                native.logical_end_with_input(
                                    &mut output,
                                    cause,
                                    &cancel,
                                    native_input_enabled.then_some(&input),
                                )?;
                            }
                        } else {
                            native.fail(
                                Error::Invalid("source finished without explicit logical EOF"),
                                cause,
                            );
                        }
                        native.complete_json(&mut output, source_events, true)?;
                        native.packet_completion_json_v1(&mut output)?;
                        if whole_object_enabled {
                            if let Some(object) = native.object.as_ref() {
                                object.evidence_json(&mut output)?;
                            }
                            // The inner object is emitted byte-for-byte in the old schema.
                            output.write_all(b"{\"kind\":\"native_object\",\"object\":")?;
                            let packet_complete =
                                !packet_completion_enabled || (!native.failed && native.finished);
                            if packet_complete
                                && native.object.as_ref().expect("whole object enabled").json(
                                    &mut output,
                                    native.failed,
                                    native.finished,
                                    native.packets,
                                )?
                            {
                                output.write_all(b",\"qualified\":false}\n")?;
                            } else {
                                output.write_all(b"null,\"error\":")?;
                                let error =
                                    native.object.as_ref().and_then(|o| o.error()).unwrap_or(
                                        Error::Unsupported("native whole object incomplete"),
                                    );
                                source_trace::json_string(&mut output, &error.to_string())?;
                                output.write_all(b",\"qualified\":false}\n")?;
                            }
                        }
                        break;
                    }
                }
                Step::Yield => {}
                Step::NeedInput(request) => {
                    if startup.failure {
                        input.fail(request.source, request.token, SourceFailure::Transport)?;
                        return Err(Error::Transport.into());
                    }
                    if startup.eof {
                        input.finish(request.source, request.token)?;
                        startup.eof = false;
                        continue;
                    }
                    let prefetched = startup.byte.take();
                    if prefetched.is_some() && request.maximum == 0 {
                        input.fail(request.source, request.token, SourceFailure::Transport)?;
                        return Err(
                            Error::Invalid("source exceeds advertised startup length").into()
                        );
                    }
                    let count = match if let Some(byte) = prefetched {
                        buffer[0] = byte;
                        file.read(&mut buffer[1..request.maximum]).map(|n| n + 1)
                    } else {
                        file.read(&mut buffer[..request.maximum])
                    } {
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
                    if whole_object_enabled {
                        let end = demux
                            .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)?
                            .ok_or("native whole object End grant yielded")?;
                        // An unsupported catalogue stays explicit; source still drains unchanged.
                        if packet_completion_enabled {
                            let _ = native.packet_physical_end_v1(&end, &mut input, &cancel);
                        } else {
                            let _ = native.object_end_with_cancel_v1(&end, &input, &cancel);
                        }
                    }
                    if let Some(next) = arguments.next() {
                        if whole_object_enabled {
                            return Err(
                                "native whole object accepts exactly one physical entity".into()
                            );
                        }
                        let end = if media_entities_enabled {
                            Some(
                                demux
                                    .media_entity_end_v1(
                                        &mut input,
                                        &mut WorkBudget::new(1),
                                        &cancel,
                                    )?
                                    .ok_or("media entity End fixed grant yielded")?,
                            )
                        } else {
                            native.fail(
                                Error::Unsupported("native entity boundary context unobserved"),
                                Cause::boundary(
                                    "entity_end_not_logical_eof",
                                    location,
                                    demux.source_epochs(),
                                ),
                            );
                            native.pending_json(&mut output)?;
                            None
                        };
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
                            if media_entities_enabled && source_clock_enabled {
                                source_clock = Some(NativePesClockV1::new(program)?);
                            }
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
                        if let Some(end) = end {
                            let cause = Cause::boundary(
                                "media_entity_transition_v1",
                                location,
                                end.epochs(),
                            );
                            let result = native.media_entity_boundary_v1(
                                &end,
                                &mut input,
                                discontinuity,
                                &mut WorkBudget::new(1),
                                &cancel,
                            );
                            let committed = match result {
                                Ok(true) => true,
                                Ok(false) => {
                                    native.fail(
                                        Error::Invalid(
                                            "media entity transaction fixed grant yielded",
                                        ),
                                        cause,
                                    );
                                    false
                                }
                                Err(error) => {
                                    native.fail(error, cause);
                                    false
                                }
                            };
                            writeln!(
                                output,
                                "{{\"kind\":\"native_media_entity_transition\",\"operation\":\"TsMediaEntitiesV1\",\"previous_source\":{},\"previous_end\":{},\"next_source\":{},\"discontinuity\":{discontinuity},\"committed\":{committed},\"qualified\":false}}",
                                location.source.0,
                                location.offset,
                                input.source().0
                            )?;
                            if committed && discontinuity && native_input_enabled {
                                startup = prepare_native_startup_v1(
                                    &mut file,
                                    &mut input,
                                    &budget,
                                    format,
                                    true,
                                    &mut buffer,
                                    &cancel,
                                )?;
                                apply_native_startup_v1(
                                    &mut native,
                                    &mut startup,
                                    &input,
                                    demux.source_epochs(),
                                );
                                native_input_operation_json(
                                    &mut output,
                                    &native,
                                    &input,
                                    startup.eof,
                                )?;
                            }
                            native.pending_json(&mut output)?;
                        }
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

#[cfg(test)]
#[path = "../tests/support/selected_lifecycle_adapter.rs"]
mod selected_lifecycle_adapter;

#[cfg(test)]
#[path = "../tests/support/media_entity_adapter_v1.rs"]
mod media_entity_adapter_v1;

#[cfg(test)]
mod native_input_abort_guard {
    use super::*;
    #[test]
    fn aborting_optional_view_releases_its_startup_alias_without_discarding_source() {
        let bytes = include_bytes!("../tests/data/native-input-v1/original-valid-rebind-192.m2ts");
        let budget = Budget::new(10000).unwrap();
        let mut input = InputCursor::new(
            &budget,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            16384,
        )
        .unwrap();
        let window = InputWindow::new(
            input.source(),
            input.token(),
            0,
            Bytes::copy_from_slice(&budget, bytes).unwrap(),
        )
        .unwrap();
        input.provide(&window).unwrap();
        let context = {
            let mut job =
                NativeInputBootstrap::new(&input, &window, PacketFormat::M2ts192).unwrap();
            job.observe_entity_eof(&input, window.source, &window.token, bytes.len() as u64)
                .unwrap();
            match job
                .poll(&input, &mut WorkBudget::new(20000), &AtomicBool::new(false))
                .unwrap()
            {
                NativeInputStep::Ready(c) => c,
                _ => panic!("finite context"),
            }
        };
        let mut native = NativeAdapter::new(budget.clone(), 1).unwrap();
        native.initial_input = Some(context);
        drop(window);
        native.fail(
            Error::Unsupported("unobserved selected map"),
            Cause::boundary(
                "actual_native_scope_refusal",
                SourceLocation {
                    source: input.source(),
                    offset: 0,
                },
                SourceEpochs::default(),
            ),
        );
        assert!(
            native.initial_input.is_none(),
            "aborted optional view must release its real startup owner"
        );
        assert_eq!(
            input.position(),
            0,
            "strict source has not been reset or consumed by native failure"
        );
        let mut work = WorkBudget::new(bytes.len() as u64);
        let mut source = vec![0; bytes.len()];
        assert!(
            matches!(input.read_into(&mut source,&mut work,&AtomicBool::new(false)).unwrap(),Step::Ready(n) if n==bytes.len())
        );
        assert_eq!(source, bytes);
        assert_eq!(work.spent(), bytes.len() as u64);
        assert_eq!(
            budget.used(),
            Budget::control_storage_bytes(),
            "only the real core input owner and optional alias existed; both now released"
        );
    }
}

#[cfg(test)]
#[path = "../tests/support/native_nonpass_object_v1.rs"]
mod native_nonpass_object_v1;

#[cfg(test)]
#[path = "../tests/support/psi_original_objects_v1.rs"]
mod psi_original_objects_v1;

#[cfg(test)]
#[path = "../tests/support/packet_process_completion_v1.rs"]
mod packet_process_completion_v1;
