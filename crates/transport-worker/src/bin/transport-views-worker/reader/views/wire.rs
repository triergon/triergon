//! Independent source checks for the reader's track and payload views.

use std::ops::Range;

use triergon_mpegts::Track;
use triergon_mpegts::reader::PayloadSource;

use super::{Entity, explain};

pub(super) fn track(entities: &[Entity], track: &Track) -> Result<(), String> {
    let map = track.program_map();
    let raw: &[u8] = &map.source;
    let mut received = raw;
    for range in map.source_ranges.as_slice() {
        let entity = range.entity.0.checked_sub(1).ok_or("PMT has no source")?;
        let entity = entities
            .get(usize::try_from(entity).map_err(explain)?)
            .ok_or("PMT source lies outside the entity sequence")?;
        let offset = usize::try_from(range.offset).map_err(explain)?;
        let bytes = entity
            .bytes
            .get(offset..offset.saturating_add(range.bytes))
            .ok_or("PMT range lies outside its physical source")?;
        received = received
            .strip_prefix(bytes)
            .ok_or("track PMT range differs from its physical source")?;
    }
    if !received.is_empty() {
        return Err("track PMT ranges omit source bytes".into());
    }
    let Some(
        [
            2,
            _,
            _,
            high,
            low,
            _,
            _,
            _,
            pcr_high,
            pcr_low,
            len_hi,
            len_lo,
        ],
    ) = raw.first_chunk::<12>()
    else {
        return Err("track has no complete wire PMT header".into());
    };
    let program = u16::from_be_bytes([*high, *low]);
    let pcr = u16::from_be_bytes([*pcr_high & 31, *pcr_low]);
    let descriptor_len = usize::from(u16::from_be_bytes([*len_hi & 15, *len_lo]));
    let sections = raw
        .get(12..raw.len().saturating_sub(4))
        .ok_or("PMT lacks CRC extent")?;
    let (descriptors, mut streams) = sections
        .split_at_checked(descriptor_len)
        .ok_or("PMT program descriptor loop is truncated")?;
    if track.program() != program
        || map.pcr_pid != pcr
        || descriptors != map.program_descriptors.as_ref()
    {
        return Err("track's PMT program fields differ from the source".into());
    }
    let stream = track.stream();
    while !streams.is_empty() {
        let ([stream_type, pid_high, pid_low, length_high, length_low], remaining) = streams
            .split_first_chunk::<5>()
            .ok_or("PMT elementary stream entry is truncated")?;
        let bytes = usize::from(u16::from_be_bytes([*length_high & 15, *length_low]));
        let (descriptors, rest) = remaining
            .split_at_checked(bytes)
            .ok_or("PMT elementary descriptor loop is truncated")?;
        if stream.pid == u16::from_be_bytes([*pid_high & 31, *pid_low]) {
            if stream.stream_type != *stream_type || stream.descriptor_bytes.as_ref() != descriptors
            {
                return Err("track's stream type or descriptors differ from the source".into());
            }
            return Ok(());
        }
        streams = rest;
    }
    Err("track's PID is absent from the retained wire PMT".into())
}

pub(super) fn payload(
    range: Range<u64>,
    mut bytes: &[u8],
    sources: &[PayloadSource],
) -> Result<(), String> {
    let mut offset = range.start;
    for source in sources {
        let payload = &source.payload_range;
        let len = payload.len();
        let pes = source
            .pes
            .event()
            .pes_source()
            .ok_or("payload range has no PES")?;
        let (span, remaining) = bytes
            .split_at_checked(len)
            .ok_or("payload source exceeds the published bytes")?;
        if payload.is_empty()
            || source.elementary_range.start != offset
            || source.elementary_range.end.saturating_sub(offset) != len as u64
            || !pes
                .payload()
                .flatten()
                .skip(payload.start)
                .take(len)
                .eq(span)
        {
            return Err("elementary range differs from the original PES bytes".into());
        }
        offset = source.elementary_range.end;
        bytes = remaining;
    }
    if range.end != offset || !bytes.is_empty() {
        return Err("elementary ranges omit published bytes".into());
    }
    Ok(())
}
