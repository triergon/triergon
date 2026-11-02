//! Test-first boundary/state batch. Parent executes before production changes.
//! Original source fixtures and EN300743 5.1.2/6.2; no native implementation.
#![forbid(unsafe_code)]

use safe_media_core::{Budget, Bytes, Error, Time, TimeBase, WorkBudget};
use safe_media_subtitles::dvb::{
    DecodeStatus, Decoder, DisplayEvent, DisplayLimits, DvbPacket, PacketEnvelope, Service,
};
use std::sync::atomic::AtomicBool;

const GENERATION: u64 = 7;
const SERVICE: Service = Service {
    composition_page: 1,
    ancillary_page: 2,
    subtitling_type: 0x14,
};
const COMPLETE: &[u8] = include_bytes!("state-fixtures/multiple-regions-objects.0.dvb");
const FIRST_REGION: &[u8] = include_bytes!("state-fixtures/multiple-regions-objects.0.1.indices");
const SPLIT: [&[u8]; 2] = [
    include_bytes!("state-fixtures/same-pts-packet-split.0.dvb"),
    include_bytes!("state-fixtures/same-pts-packet-split.1.dvb"),
];
const PLACEMENT: [&[u8]; 2] = [
    include_bytes!("state-fixtures/placement-only.0.dvb"),
    include_bytes!("state-fixtures/placement-only.1.dvb"),
];

fn decoder(budget: &Budget, cancel: &AtomicBool) -> Decoder {
    Decoder::new(
        budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        cancel,
    )
    .unwrap()
}

fn packet(budget: &Budget, bytes: &[u8], sequence: u64, envelope: PacketEnvelope) -> DvbPacket {
    DvbPacket {
        payload: Bytes::copy_from_slice(budget, bytes).unwrap(),
        pts: Some(Time::from_ticks(90000, TimeBase::new(1, 90000).unwrap()).unwrap()),
        generation: GENERATION,
        sequence,
        envelope,
        source: None,
    }
}

fn pes(segments: &[u8]) -> Vec<u8> {
    let mut payload = vec![0x20, 0];
    payload.extend_from_slice(segments);
    payload.push(0xff);
    payload
}

struct Observed {
    events: Vec<DisplayEvent>,
    error: Option<Error>,
}

fn consume(decoder: &mut Decoder, units: u64, cancel: &AtomicBool) -> Observed {
    let mut observed = Observed {
        events: Vec::new(),
        error: None,
    };
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(units);
        match decoder.step(&mut work, cancel) {
            Err(error) => {
                observed.error = Some(error);
                return observed;
            }
            Ok(progress) => match progress.status {
                DecodeStatus::Event => observed.events.push(progress.event.unwrap()),
                DecodeStatus::Yield => assert!(work.spent() > 0),
                DecodeStatus::NeedPacket | DecodeStatus::Finished => return observed,
                DecodeStatus::NeedOutput => panic!("small authored case exceeded generous pool"),
            },
        }
    }
    panic!("bounded authored packet did not reach a boundary");
}

#[test]
fn empty_segments_are_not_a_drain_or_entity_end() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = decoder(&budget, &cancel);
    let source = packet(&budget, &[], 0, PacketEnvelope::Segments);
    assert_eq!(
        decoder.provide(&source).unwrap_err(),
        Error::Invalid("DVB empty segment packet")
    );
}

#[test]
fn empty_pes_data_field_is_not_a_display() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = decoder(&budget, &cancel);
    let source = packet(&budget, &[0x20, 0, 0xff], 0, PacketEnvelope::PesDataField);
    assert_eq!(
        decoder.provide(&source).unwrap_err(),
        Error::Invalid("DVB empty PES data field")
    );
}

#[test]
fn pes_second_selected_display_set_preserves_first_event_then_fails() {
    let raw = COMPLETE;
    let mut two_sets = raw.to_vec();
    two_sets.extend_from_slice(raw);
    for units in [1, 7, 4096] {
        let budget = Budget::new(4 * 1024 * 1024).unwrap();
        let cancel = AtomicBool::new(false);
        let mut decoder = decoder(&budget, &cancel);
        let source = packet(&budget, &pes(&two_sets), 0, PacketEnvelope::PesDataField);
        decoder.provide(&source).unwrap();
        let observed = consume(&mut decoder, units, &cancel);
        assert_eq!(observed.events.len(), 1, "EN300743 5.1.2, units={units}");
        assert_eq!(
            observed.error,
            Some(Error::Invalid(
                "DVB multiple display sets in PES data field"
            ))
        );
        assert_eq!(observed.events[0].regions().len(), 2);
        let retained = observed.events[0].regions()[0].plane.pixels.clone();
        drop(decoder);
        assert_eq!(retained.as_ref(), FIRST_REGION);
    }
}

#[test]
fn increasing_exact_pts_need_no_representable_subtraction() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let initial_charge = budget.used();
    let cancel = AtomicBool::new(false);
    let mut decoder = decoder(&budget, &cancel);
    let raw = COMPLETE;
    let mut first = packet(&budget, raw, 0, PacketEnvelope::Segments);
    first.pts = Some(Time::new(i128::MIN, 1).unwrap());
    decoder.provide(&first).unwrap();
    let before = consume(&mut decoder, 7, &cancel);
    assert_eq!(before.error, None);
    assert_eq!(before.events.len(), 1);
    assert_eq!(before.events[0].pts, first.pts);
    let mut second = packet(&budget, raw, 1, PacketEnvelope::Segments);
    second.pts = Some(Time::new(i128::MAX - 2, 1).unwrap());
    assert!(first.pts.unwrap() < second.pts.unwrap());
    decoder.provide(&second).unwrap();
    let after = consume(&mut decoder, 7, &cancel);
    assert_eq!(after.error, None);
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.events[0].pts, second.pts);
    assert_eq!(
        after.events[0].expiry,
        Some(Time::new(i128::MAX, 1).unwrap())
    );
    drop(decoder);
    assert_eq!(before.events[0].pts, first.pts);
    drop(before);
    drop(after);
    drop(first);
    drop(second);
    assert_eq!(budget.used(), initial_charge);
}

#[test]
fn declared_segments_never_autodetect_a_pes_envelope() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = decoder(&budget, &cancel);
    let raw = COMPLETE;
    let source = packet(&budget, &pes(raw), 0, PacketEnvelope::Segments);
    decoder.provide(&source).unwrap();
    let observed = consume(&mut decoder, 1, &cancel);
    assert!(observed.events.is_empty());
    assert_eq!(observed.error, Some(Error::Invalid("DVB segment sync")));
}

#[test]
fn a_pes_marker_does_not_finish_a_split_display_set() {
    for units in [1, 7, 4096] {
        let budget = Budget::new(4 * 1024 * 1024).unwrap();
        let cancel = AtomicBool::new(false);
        let mut decoder = decoder(&budget, &cancel);
        let first = packet(&budget, &pes(SPLIT[0]), 0, PacketEnvelope::PesDataField);
        decoder.provide(&first).unwrap();
        let before = consume(&mut decoder, units, &cancel);
        assert_eq!(before.error, None);
        assert!(before.events.is_empty());
        let second = packet(&budget, &pes(SPLIT[1]), 1, PacketEnvelope::PesDataField);
        decoder.provide(&second).unwrap();
        let after = consume(&mut decoder, units, &cancel);
        assert_eq!(after.error, None);
        assert_eq!(after.events.len(), 1);
        assert_eq!(after.events[0].source_packets().len(), 2);
        assert_eq!(
            after.events[0].source_packets()[0].payload.as_ref(),
            first.payload.as_ref()
        );
        assert_eq!(
            after.events[0].source_packets()[1].payload.as_ref(),
            second.payload.as_ref()
        );
    }
}

#[test]
fn unknown_update_pts_is_not_filled_from_the_previous_packet() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = decoder(&budget, &cancel);
    let first = packet(&budget, PLACEMENT[0], 0, PacketEnvelope::Segments);
    decoder.provide(&first).unwrap();
    let before = consume(&mut decoder, 7, &cancel);
    assert_eq!(before.error, None);
    assert_eq!(before.events.len(), 1);
    let mut second = packet(&budget, PLACEMENT[1], 1, PacketEnvelope::Segments);
    second.pts = None;
    decoder.provide(&second).unwrap();
    let after = consume(&mut decoder, 7, &cancel);
    assert_eq!(after.error, None);
    assert_eq!(after.events.len(), 1);
    assert_eq!(after.events[0].pts, None);
    assert_eq!(after.events[0].expiry, None);
    assert_eq!(after.events[0].source_packets()[0].pts, None);
}

#[test]
fn raw_malformed_source_tail_is_not_replaced_with_the_native_ts_prefix() {
    let raw = include_bytes!("boundary-fixtures/original-truncated-segment.dvb");
    assert_eq!(raw.len(), 106);
    assert_eq!(&raw[101..], &[0x0f, 0x80, 0, 1, 0]);
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = decoder(&budget, &cancel);
    let source = packet(&budget, raw, 0, PacketEnvelope::Segments);
    decoder.provide(&source).unwrap();
    decoder.finish_input().unwrap();
    let observed = consume(&mut decoder, 1, &cancel);
    assert_eq!(source.payload.as_ref(), raw);
    assert!(observed.events.is_empty());
    assert_eq!(observed.error, Some(Error::Truncated));
}
