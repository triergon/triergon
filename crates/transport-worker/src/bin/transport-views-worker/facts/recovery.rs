//! Grade recovery witnesses alongside the native packet fold.

use std::sync::atomic::AtomicBool;

use triergon_core::{Budget, Error, WorkBudget};
use triergon_mpegts::SourceRange;
use triergon_mpegts::compat::pes::{DemuxSpan, PesFaultPacket, SourceContext};
use triergon_mpegts::psi::{PatTable, ProgramAssociation, TableStep};
use triergon_mpegts::reference_trace::recovery::{PatRoute, PatSelection, RecoveredPacket};

use super::{SOURCE, Stop, WORK};

pub(super) fn expect_pending<T>(
    mut poll: impl FnMut(&mut WorkBudget, &AtomicBool) -> triergon_core::Result<TableStep<T>>,
) -> Result<(), Stop> {
    if !matches!(
        poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(TableStep::Yield)
    ) || !matches!(
        poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ) {
        return Err("table job changed its work or cancellation outcome".into());
    }
    Ok(())
}

pub(super) fn check_pat(
    table: &PatTable,
    requested: Option<&ProgramAssociation>,
    route: Option<PatRoute>,
) -> Result<(), Stop> {
    let count = table
        .programs
        .as_slice()
        .iter()
        .filter(|entry| entry.program_number != 0)
        .count();
    match (requested, route) {
        (None, None) => Ok(()),
        (Some(entry), Some(route))
            if route.entity() == SOURCE
                && route.program() == entry.program_number
                && route.pmt_pid() == entry.pid
                && route.program_count() == count =>
        {
            Ok(())
        }
        _ => Err("PAT selection differs from the original association list".into()),
    }
}

pub(super) fn check_pat_selection(
    table: &PatTable,
    pool: &Budget,
    context: SourceContext,
) -> Result<(), Stop> {
    if !matches!(
        PatSelection::new(table, 0, pool, context),
        Err(Error::Invalid(
            "selected program zero is a network association"
        ))
    ) {
        return Err("PAT selector admitted a network association as a program".into());
    }
    let missing = (1..=u16::MAX)
        .rev()
        .find(|&number| {
            table
                .programs
                .as_slice()
                .iter()
                .all(|entry| entry.program_number != number)
        })
        .ok_or("PAT control has no unlisted program")?;
    let mut selection = PatSelection::new(table, missing, pool, context)?;
    if !matches!(
        selection.poll(&mut WorkBudget::new(WORK), &AtomicBool::new(false)),
        Ok(TableStep::Output(None))
    ) {
        return Err("PAT selector published a route for an unlisted program".into());
    }
    Ok(())
}

fn original(source: &[u8], range: SourceRange) -> Option<&[u8]> {
    if range.entity != SOURCE {
        return None;
    }
    let start = usize::try_from(range.offset).ok()?;
    source.get(start..start.checked_add(range.bytes)?)
}

fn check_header(source: &[u8], mut header: &[u8], ranges: &[SourceRange]) -> Result<(), Stop> {
    for range in ranges {
        let wire = original(source, *range).ok_or("header range lies outside the source")?;
        header = header
            .strip_prefix(wire)
            .ok_or("encoded header differs from source bytes")?;
    }
    if !header.is_empty() {
        return Err("source ranges omit encoded header bytes".into());
    }
    Ok(())
}

fn check_spans(source: &[u8], spans: &[DemuxSpan], length: usize) -> Result<(), Stop> {
    let mut count = 0;
    for span in spans {
        if original(source, span.source) != Some(span.as_ref())
            || original(source, span.transport).is_none()
            || span.source.offset < span.transport.offset
            || span.source.offset + span.source.bytes as u64
                > span.transport.offset + span.transport.bytes as u64
        {
            return Err("recovery span differs from its original carrier bytes".into());
        }
        count += span.source.bytes;
    }
    if count != length {
        return Err("recovery spans do not cover the published payload".into());
    }
    Ok(())
}

pub(super) fn check_rejected(
    source: &[u8],
    packet: &RecoveredPacket,
    program: u16,
) -> Result<(), Stop> {
    let range = packet
        .rejection
        .source_range()
        .ok_or("rejection has no original range")?;
    let wire = original(source, range).ok_or("rejected carrier lies outside the source")?;
    let sync = wire.len() - 188;
    if packet.program != program
        || packet.source_origin.source != SOURCE
        || packet.source_origin.offset != range.offset
        || packet.source_epochs != packet.context.epochs
        || wire != packet.rejection.bytes().as_ref()
        || packet.pid != u16::from_be_bytes([wire[sync + 1] & 31, wire[sync + 2]])
        || packet.encoded_header.as_bytes().get(3) != Some(&packet.stream_id)
    {
        return Err("rejection lost its source carrier, scope or context".into());
    }
    check_header(
        source,
        packet.encoded_header.as_bytes(),
        packet.header_source_ranges.as_slice(),
    )?;
    check_spans(source, packet.spans.as_slice(), packet.payload_len)
}

pub(super) fn check_diagnostic(
    source: &[u8],
    packet: &PesFaultPacket,
    program: u16,
) -> Result<(), Stop> {
    let carrier = packet.carrier.backing_bytes();
    let range = SourceRange {
        entity: packet.source_origin.source,
        offset: packet.source_origin.offset,
        bytes: carrier.len(),
    };
    let header = packet.encoded_header.as_bytes();
    let marker_bits = header
        .get(9..14)
        .is_some_and(|pts| [pts[0], pts[2], pts[4]].iter().all(|byte| byte & 1 != 0));
    let flags = header
        .get(7)
        .ok_or("diagnostic PES header has no timing flags")?
        >> 6;
    if packet.program != program
        || original(source, range) != Some(carrier.as_ref())
        || packet.pid != u16::from_be_bytes([carrier[1] & 31, carrier[2]])
        || header.get(3) != Some(&packet.stream_id)
        || packet.encoded_header.time_flags() != flags
        || packet.encoded_header.markers_valid() != (flags == 2).then_some(marker_bits)
        || packet.source_fault.location != packet.source_origin
        || packet.source_fault.kind != packet.kind.source_layer()
        || packet.source_fault.error != packet.kind.source_error()
        || packet.input_context.epochs != packet.source_epochs
        || packet.input_context.source_event_ordinal
            > packet.diagnostic_context.source_event_ordinal
    {
        return Err("diagnostic recovery differs from its carrier, fault or context".into());
    }
    check_header(source, header, packet.header_source_ranges.as_slice())?;
    check_spans(source, packet.spans.as_slice(), packet.payload_len)
}
