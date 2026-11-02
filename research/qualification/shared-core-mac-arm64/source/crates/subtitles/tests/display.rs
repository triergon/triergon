//! Independent complete-state source art precedes connecting the production modules.
//! Authored in the ignored lane before module promotion; parent executes Rust.
#![forbid(unsafe_code)]

#[path = "support/state_cases.rs"]
mod state_cases;

use safe_media_core::{Budget, Bytes, Error, Time, TimeBase, WorkBudget};
use safe_media_subtitles::dvb::{
    ColourSource, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, DvbPacket, ObjectRetention,
    PacketEnvelope, SOURCE_PROJECTION, Service,
};
use std::sync::atomic::{AtomicBool, Ordering};

const GENERATION: u64 = 7;
const SERVICE: Service = Service {
    composition_page: 1,
    ancillary_page: 2,
    subtitling_type: 0x14,
};

fn packet(budget: &Budget, source: &state_cases::Packet, sequence: usize) -> DvbPacket {
    DvbPacket {
        payload: Bytes::copy_from_slice(budget, source.bytes).unwrap(),
        pts: Some(Time::from_ticks(source.pts_ticks, TimeBase::new(1, 90000).unwrap()).unwrap()),
        generation: GENERATION,
        sequence: u64::try_from(sequence).unwrap(),
        envelope: PacketEnvelope::Segments,
        source: None,
    }
}

fn first_event(decoder: &mut Decoder, units: u64, cancel: &AtomicBool) -> DisplayEvent {
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(units);
        let progress = decoder.step(&mut work, cancel).unwrap();
        match progress.status {
            DecodeStatus::Event => return progress.event.expect("atomic complete display event"),
            DecodeStatus::Yield => assert!(work.spent() > 0),
            status => panic!("unexpected status before supplied complete display: {status:?}"),
        }
    }
    panic!("bounded source fixture did not complete");
}

fn assert_event(actual: &DisplayEvent, expected: &state_cases::Event) {
    assert_eq!(actual.service, SERVICE);
    assert_eq!(actual.generation, GENERATION);
    assert_eq!(actual.action, expected.action);
    let pts = Time::from_ticks(expected.pts_ticks, TimeBase::new(1, 90000).unwrap()).unwrap();
    assert_eq!(actual.pts, Some(pts));
    assert_eq!(actual.timeout_seconds, 2);
    assert_eq!(
        actual.expiry,
        Some(pts.checked_add(Time::new(2, 1).unwrap()).unwrap())
    );
    assert_eq!(actual.definition.size, expected.canvas);
    assert_eq!(actual.regions().len(), expected.regions.len());
    for (region, source) in actual.regions().iter().zip(expected.regions) {
        assert_eq!(region.id, source.id);
        assert_eq!(region.position, source.position);
        assert_eq!(region.size, source.size);
        assert_eq!(region.plane.pixels.as_ref(), source.indices);
        assert_eq!(region.palette.projection, SOURCE_PROJECTION);
        let palette = region.palette.rgba.as_ref();
        let expanded: Vec<u8> = region
            .plane
            .pixels
            .as_ref()
            .iter()
            .flat_map(|&index| {
                let at = usize::from(index) * 4;
                palette[at..at + 4].iter().copied()
            })
            .collect();
        assert_eq!(expanded, source.rgba);
    }
}

#[test]
fn complete_state_cases_match_original_art_with_tiny_work() {
    for case in state_cases::CASES {
        for units in [1, 7, 4096] {
            let budget = Budget::new(4 * 1024 * 1024).unwrap();
            let initial_charge = budget.used();
            let cancel = AtomicBool::new(false);
            let retention = if case.id == "reuse-cached-object" {
                ObjectRetention::EpochExtension
            } else {
                ObjectRetention::CurrentDisplay
            };
            let mut decoder = Decoder::new_with_object_retention(
                &budget,
                SERVICE,
                GENERATION,
                DisplayLimits::default(),
                retention,
                &cancel,
            )
            .unwrap();
            let mut next = 0;
            let mut events = Vec::new();
            let mut failure = None;
            let mut finished = false;
            let mut end_sent = false;
            for _ in 0..1_000_000 {
                let mut work = WorkBudget::new(units);
                let progress = match decoder.step(&mut work, &cancel) {
                    Ok(progress) => progress,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                };
                match progress.status {
                    DecodeStatus::NeedPacket => {
                        if let Some(source) = case.packets.get(next) {
                            decoder.provide(&packet(&budget, source, next)).unwrap();
                            next += 1;
                        } else {
                            assert!(!end_sent, "{} repeated input request after finish", case.id);
                            decoder.finish_input().unwrap();
                            end_sent = true;
                        }
                    }
                    DecodeStatus::Yield => assert!(work.spent() > 0),
                    DecodeStatus::Event => events.push(progress.event.unwrap()),
                    DecodeStatus::NeedOutput => {
                        panic!("{} unexpectedly exceeded generous test pool", case.id)
                    }
                    DecodeStatus::Finished => {
                        finished = true;
                        break;
                    }
                }
            }
            assert_eq!(events.len(), case.events.len(), "{} units={units}", case.id);
            for (number, (actual, expected)) in events.iter().zip(case.events).enumerate() {
                assert_eq!(actual.sequence, u64::try_from(number).unwrap());
                assert_event(actual, expected);
                let sources = actual.source_packets();
                assert!(!sources.is_empty());
                for source in sources {
                    let expected_packet = &case.packets[usize::try_from(source.sequence).unwrap()];
                    assert_eq!(source.payload.as_ref(), expected_packet.bytes);
                }
            }
            assert_eq!(
                failure,
                case.error.map(Error::Invalid),
                "{} units={units}",
                case.id
            );
            assert_eq!(finished, case.error.is_none(), "{} units={units}", case.id);
            // Retained events are independent of the mutable decoder/cache lifetime.
            drop(decoder);
            for (actual, expected) in events.iter().zip(case.events) {
                assert_event(actual, expected);
            }
            drop(events);
            assert_eq!(
                budget.used(),
                initial_charge,
                "{} released retained charges",
                case.id
            );
        }
    }
}

#[test]
fn zero_work_and_entity_end_do_not_synthesize_eod() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let mut source = packet(&budget, &state_cases::CASES[0].packets[0], 0);
    source.payload = source.payload.slice(0..source.payload.len() - 6).unwrap();
    decoder.provide(&source).unwrap();
    let mut empty_work = WorkBudget::new(0);
    assert_eq!(
        decoder.step(&mut empty_work, &cancel).unwrap().status,
        DecodeStatus::Yield
    );
    assert_eq!(empty_work.spent(), 0);
    for _ in 0..100_000 {
        let progress = decoder.step(&mut WorkBudget::new(7), &cancel).unwrap();
        if progress.status == DecodeStatus::NeedPacket {
            break;
        }
        assert_eq!(progress.status, DecodeStatus::Yield);
    }
    decoder.finish_input().unwrap();
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Truncated
    );
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Truncated
    );
}

#[test]
fn allocation_pressure_preserves_the_exact_transaction_for_retry() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let source = packet(&budget, &state_cases::CASES[0].packets[0], 0);
    decoder.provide(&source).unwrap();
    let blocker = budget.reserve(budget.available()).unwrap();
    let progress = decoder.step(&mut WorkBudget::new(4096), &cancel).unwrap();
    assert_eq!(progress.status, DecodeStatus::NeedOutput);
    assert!(matches!(
        progress.pressure,
        Some(Error::Backpressure { .. })
    ));
    assert!(progress.event.is_none());
    let again = decoder.step(&mut WorkBudget::new(4096), &cancel).unwrap();
    assert_eq!(again.status, DecodeStatus::NeedOutput);
    drop(blocker);
    let event = first_event(&mut decoder, 1, &cancel);
    assert_event(&event, &state_cases::CASES[0].events[0]);
}

#[test]
fn cancellation_poisons_private_work_and_requires_explicit_reset() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let source = packet(&budget, &state_cases::CASES[0].packets[0], 0);
    decoder.provide(&source).unwrap();
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(50), &cancel)
            .unwrap()
            .status,
        DecodeStatus::Yield
    );
    let before = budget.used();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    assert!(budget.used() <= before);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    decoder.reset(GENERATION + 1, &cancel).unwrap();
    let mut new_source = source.clone();
    new_source.generation += 1;
    decoder.provide(&new_source).unwrap();
    let event = first_event(&mut decoder, 7, &cancel);
    assert_eq!(event.generation, GENERATION + 1);
    assert_eq!(
        event.regions()[0].plane.pixels.as_ref(),
        state_cases::CASES[0].events[0].regions[0].indices
    );
}

#[test]
fn input_generation_pool_and_reset_do_not_corrupt_retained_output() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let mut source = packet(&foreign, &state_cases::CASES[0].packets[0], 0);
    assert_eq!(decoder.provide(&source).unwrap_err(), Error::ForeignPool);
    source = packet(&budget, &state_cases::CASES[0].packets[0], 0);
    source.generation += 1;
    assert_eq!(
        decoder.provide(&source).unwrap_err(),
        Error::StaleGeneration {
            expected: GENERATION,
            received: GENERATION + 1,
        }
    );
    source.generation = GENERATION;
    decoder.provide(&source).unwrap();
    let retained = first_event(&mut decoder, 7, &cancel);
    decoder.reset(GENERATION + 1, &cancel).unwrap();
    assert_event(&retained, &state_cases::CASES[0].events[0]);
    assert_eq!(
        decoder.provide(&source).unwrap_err(),
        Error::StaleGeneration {
            expected: GENERATION + 1,
            received: GENERATION,
        }
    );
    source.generation += 1;
    decoder.provide(&source).unwrap();
    let next = first_event(&mut decoder, 7, &cancel);
    assert_eq!(next.epoch, 1);
    assert_eq!(next.sequence, 0);
    drop(decoder);
    assert_event(&retained, &state_cases::CASES[0].events[0]);
}

#[test]
fn source_clut_components_and_private_segments_remain_available() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let case = state_cases::CASES
        .iter()
        .find(|case| case.id == "private-extension-preserved")
        .unwrap();
    let source = packet(&budget, &case.packets[0], 0);
    decoder.provide(&source).unwrap();
    let event = first_event(&mut decoder, 1, &cancel);
    let palette = &event.regions()[0].palette;
    assert_eq!(
        palette.entries()[0].source,
        ColourSource::YCrCbT {
            y: 0,
            cr: 0,
            cb: 0,
            transparency: 0,
            full_resolution: true,
        }
    );
    assert_eq!(
        palette.entries()[1].source,
        ColourSource::YCrCbT {
            y: 235,
            cr: 128,
            cb: 128,
            transparency: 0,
            full_resolution: true,
        }
    );
    assert_eq!(&palette.rgba.as_ref()[..4], &[0, 0, 0, 0]);
    assert_eq!(
        &palette.studio_unrepaired_rgba.as_ref()[..4],
        &[0, 136, 0, 0]
    );
    assert_ne!(palette.projection, palette.studio_unrepaired_projection);
    assert_eq!(
        &palette.entries()[0]
            .original_segment
            .as_ref()
            .unwrap()
            .as_ref()[..2],
        &[15, 18]
    );
    assert_eq!(event.opaque_segments().len(), 1);
    let opaque = &event.opaque_segments()[0];
    assert_eq!(opaque.kind, 0x81);
    assert!(!opaque.unsupported_feature);
    assert_eq!(&opaque.original.as_ref()[6..], &[0x18, 0x2d, 0x53]);
    assert_eq!(
        event.source_packets()[0].payload.as_ref(),
        source.payload.as_ref()
    );
}

#[test]
fn display_limits_fail_explicitly_without_publishing_partial_regions() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let source = packet(&budget, &state_cases::CASES[0].packets[0], 0);
    for limits in [
        DisplayLimits {
            aggregate_region_pixels: 11,
            ..DisplayLimits::default()
        },
        DisplayLimits {
            segments_per_display: 1,
            ..DisplayLimits::default()
        },
        DisplayLimits {
            work_per_display: 3,
            ..DisplayLimits::default()
        },
    ] {
        let mut decoder = Decoder::new(&budget, SERVICE, GENERATION, limits, &cancel).unwrap();
        decoder.provide(&source).unwrap();
        let mut failure = None;
        for _ in 0..100_000 {
            match decoder.step(&mut WorkBudget::new(1), &cancel) {
                Err(error) => {
                    failure = Some(error);
                    break;
                }
                Ok(progress) => {
                    assert_eq!(progress.status, DecodeStatus::Yield);
                    assert!(progress.event.is_none());
                }
            }
        }
        assert!(matches!(failure, Some(Error::Limit { .. })));
    }
}

#[test]
fn colour_quantization_is_named_and_preserves_exact_source_opacity() {
    let half = ColourSource::YCrCbT {
        y: 235,
        cr: 128,
        cb: 128,
        transparency: 128,
        full_resolution: true,
    };
    assert_eq!(half.opacity().unwrap(), (128, 256));
    assert_eq!(half.rgba_source_nearest().unwrap(), [255, 255, 255, 128]);
    let reduced = ColourSource::YCrCbT {
        y: 4,
        cr: 8,
        cb: 8,
        transparency: 2,
        full_resolution: false,
    };
    assert_eq!(reduced.opacity().unwrap(), (2, 4));
    assert_eq!(reduced.rgba_source_nearest().unwrap(), [0, 0, 0, 128]);
    let transparent = ColourSource::YCrCbT {
        y: 0,
        cr: 0,
        cb: 0,
        transparency: 0,
        full_resolution: true,
    };
    assert_eq!(transparent.opacity().unwrap(), (0, 1));
    // This is the explicitly named source projection, not a native RGB normalizer.
    assert_eq!(transparent.rgba_source_nearest().unwrap(), [0, 0, 0, 0]);
}

#[test]
fn overlapping_current_object_codes_are_rejected() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut bytes = state_cases::CASES[0].packets[0].bytes.to_vec();
    // Author one explicit negative variant: object 2 starts at x=1 instead of
    // x=2 in the first RCS. Both width-two objects now code source column one.
    let mut at = 0_usize;
    let mut changed = false;
    while at < bytes.len() {
        let length = usize::from(u16::from_be_bytes([bytes[at + 4], bytes[at + 5]]));
        if bytes[at + 1] == 0x11 && bytes[at + 6] == 1 {
            let second_object_x = at + 6 + 10 + 6 + 2;
            bytes[second_object_x..second_object_x + 2].copy_from_slice(&1_u16.to_be_bytes());
            changed = true;
            break;
        }
        at += 6 + length;
    }
    assert!(changed);
    let source = DvbPacket {
        payload: Bytes::copy_from_slice(&budget, &bytes).unwrap(),
        ..packet(&budget, &state_cases::CASES[0].packets[0], 0)
    };
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    decoder.provide(&source).unwrap();
    for _ in 0..100_000 {
        match decoder.step(&mut WorkBudget::new(1), &cancel) {
            Err(error) => {
                assert_eq!(error, Error::Invalid("DVB current objects overlap"));
                return;
            }
            Ok(progress) => {
                assert_eq!(progress.status, DecodeStatus::Yield);
                assert!(progress.event.is_none());
            }
        }
    }
    panic!("authored overlap must fail within the bounded fixture work");
}

#[test]
fn pes_data_field_envelope_is_explicit() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let original = &state_cases::CASES[0].packets[0];
    let mut wrapped = vec![0x20, 0];
    wrapped.extend_from_slice(original.bytes);
    wrapped.push(0xff);
    let mut source = packet(&budget, original, 0);
    source.payload = Bytes::copy_from_slice(&budget, &wrapped).unwrap();
    source.envelope = PacketEnvelope::PesDataField;
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    decoder.provide(&source).unwrap();
    let event = first_event(&mut decoder, 7, &cancel);
    assert_event(&event, &state_cases::CASES[0].events[0]);
    assert_eq!(event.source_packets()[0].payload.as_ref(), wrapped);
    // The same bytes with a different declared envelope fail visibly. Public
    // AVCodec's observed rejection of this envelope remains a separate finding.
    source.envelope = PacketEnvelope::Segments;
    let mut raw = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    raw.provide(&source).unwrap();
    assert_eq!(
        raw.step(&mut WorkBudget::new(4096), &cancel).unwrap_err(),
        Error::Invalid("DVB segment sync")
    );
}

#[test]
fn exact_optional_pts_and_same_pts_display_set_constraint() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    for pts in [Some(Time::new(-1, 1000).unwrap()), None] {
        let mut source = packet(&budget, &state_cases::CASES[0].packets[0], 0);
        source.pts = pts;
        let mut decoder = Decoder::new(
            &budget,
            SERVICE,
            GENERATION,
            DisplayLimits::default(),
            &cancel,
        )
        .unwrap();
        decoder.provide(&source).unwrap();
        let event = first_event(&mut decoder, 7, &cancel);
        assert_eq!(event.pts, pts);
        assert_eq!(
            event.expiry,
            pts.map(|time| time.checked_add(Time::new(2, 1).unwrap()).unwrap())
        );
    }
    let split = state_cases::CASES
        .iter()
        .find(|case| case.id == "same-pts-packet-split")
        .unwrap();
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    decoder
        .provide(&packet(&budget, &split.packets[0], 0))
        .unwrap();
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap()
            .status,
        DecodeStatus::NeedPacket
    );
    let mut second = packet(&budget, &split.packets[1], 1);
    second.pts = Some(Time::new(2, 1).unwrap());
    decoder.provide(&second).unwrap();
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Invalid("DVB display-set PTS changed")
    );
}

#[test]
fn cross_display_object_reuse_requires_an_explicit_extension_profile() {
    let case = state_cases::CASES
        .iter()
        .find(|case| case.id == "reuse-cached-object")
        .unwrap();
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(
        &budget,
        SERVICE,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    decoder
        .provide(&packet(&budget, &case.packets[0], 0))
        .unwrap();
    let first = first_event(&mut decoder, 7, &cancel);
    assert_eq!(first.object_retention, ObjectRetention::CurrentDisplay);
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap()
            .status,
        DecodeStatus::NeedPacket
    );
    decoder
        .provide(&packet(&budget, &case.packets[1], 1))
        .unwrap();
    for _ in 0..100_000 {
        match decoder.step(&mut WorkBudget::new(1), &cancel) {
            Err(error) => {
                assert_eq!(error, Error::Invalid("DVB missing object data"));
                assert_event(&first, &case.events[0]);
                return;
            }
            Ok(progress) => assert_eq!(progress.status, DecodeStatus::Yield),
        }
    }
    panic!("current-display policy must not silently reuse an earlier ODS");
}
