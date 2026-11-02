//! Original configured-boundary tests authored before its module connection.
//! Promoted after the parent's make10 gate; all nine first passed in make11.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, DvbPacket,
    PacketEnvelope, PacketIdentity, PacketTiming, SourceClock, SourceConfiguration,
};

const GENERATION: u64 = 7;
const PRIVATE: [u8; 5] = [0, 1, 0, 2, 0x14];
const DESCRIPTOR: [u8; 10] = [0x59, 8, b'e', b'n', b'g', 0x14, 0, 1, 0, 2];
const COMPLETE: &[u8] = include_bytes!("boundary-fixtures/dvb2-literals-fields.dvb");

struct Fixture {
    bytes: &'static [u8],
    indices: &'static [u8],
    rgba: &'static [u8],
    subtype: u8,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        bytes: COMPLETE,
        indices: include_bytes!("boundary-fixtures/dvb2-literals-fields.indices"),
        rgba: include_bytes!("boundary-fixtures/dvb2-literals-fields.rgba"),
        subtype: 0x14,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-ancillary-pages.dvb"),
        indices: include_bytes!("boundary-fixtures/dvb-ancillary-pages.indices"),
        rgba: include_bytes!("boundary-fixtures/dvb-ancillary-pages.rgba"),
        subtype: 0x14,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-default-clut2.dvb"),
        indices: include_bytes!("boundary-fixtures/dvb-default-clut2.indices"),
        rgba: include_bytes!("boundary-fixtures/dvb-default-clut2.rgba"),
        subtype: 0x10,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-nonmodifying.dvb"),
        indices: include_bytes!("boundary-fixtures/dvb-nonmodifying.indices"),
        rgba: include_bytes!("boundary-fixtures/dvb-nonmodifying.rgba"),
        subtype: 0x14,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-partial-alpha-projection.dvb"),
        indices: include_bytes!("boundary-fixtures/dvb-partial-alpha-projection.indices"),
        rgba: include_bytes!("boundary-fixtures/dvb-partial-alpha-projection.rgba"),
        subtype: 0x14,
    },
];

fn identity(sequence: u64) -> PacketIdentity {
    PacketIdentity {
        generation: GENERATION,
        sequence,
        container_position: Some(SourceLocation {
            source: SourceId(31),
            offset: 280,
        }),
    }
}

fn event(decoder: &mut Decoder, cancel: &AtomicBool) -> DisplayEvent {
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let next = decoder.step(&mut work, cancel).unwrap();
        match next.status {
            DecodeStatus::Event => return next.event.unwrap(),
            DecodeStatus::Yield => assert!(work.spent() > 0),
            status => panic!("configured source did not emit its complete event: {status:?}"),
        }
    }
    panic!("bounded configured source did not finish");
}

fn configured_packet(
    budget: &Budget,
    configuration: &SourceConfiguration,
    ticks: i128,
) -> DvbPacket {
    let payload = Bytes::copy_from_slice(budget, COMPLETE).unwrap();
    configuration
        .packet(
            budget,
            &payload,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks,
                    origin: ClockOrigin::Source,
                }),
                dts: None,
                duration: None,
            },
            identity(0),
        )
        .unwrap()
}

#[test]
fn source_private_descriptor_and_clock_are_explicit_and_preserved() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    for fixture in FIXTURES {
        let mut private = PRIVATE;
        private[4] = fixture.subtype;
        let mut descriptor = DESCRIPTOR;
        descriptor[5] = fixture.subtype;
        for (bytes, numerator, denominator) in
            [(&private[..], 1, 1000), (&descriptor[..], 1, 90000)]
        {
            let original = Bytes::copy_from_slice(&budget, bytes).unwrap();
            let clock = SourceClock::new(numerator, denominator).unwrap();
            let configuration = if bytes.len() == 5 {
                SourceConfiguration::from_matroska_private(&budget, &original, clock).unwrap()
            } else {
                SourceConfiguration::from_subtitling_descriptor(
                    &budget,
                    &original,
                    0,
                    clock,
                    PacketEnvelope::Segments,
                )
                .unwrap()
            };
            assert_eq!(configuration.original().as_ref(), bytes);
            assert_eq!(configuration.service().composition_page, 1);
            assert_eq!(configuration.service().ancillary_page, 2);
            assert_eq!(configuration.service().subtitling_type, fixture.subtype);
            assert_eq!(configuration.service_private_view(), private);
            assert_eq!(configuration.clock().raw_numerator(), numerator);
            assert_eq!(configuration.clock().raw_denominator(), denominator);
            assert_eq!(configuration.envelope(), PacketEnvelope::Segments);
            let payload = Bytes::copy_from_slice(&budget, fixture.bytes).unwrap();
            let packet = configuration
                .packet(
                    &budget,
                    &payload,
                    PacketTiming {
                        pts: Some(ClockValue {
                            ticks: i128::from(denominator),
                            origin: ClockOrigin::Source,
                        }),
                        ..PacketTiming::default()
                    },
                    identity(0),
                )
                .unwrap();
            let mut decoder = Decoder::new_configured(
                &budget,
                &configuration,
                GENERATION,
                DisplayLimits::default(),
                &cancel,
            )
            .unwrap();
            decoder.provide(&packet).unwrap();
            let output = event(&mut decoder, &cancel);
            assert_eq!(output.pts, Some(Time::new(1, 1).unwrap()));
            let region = &output.regions()[0];
            assert_eq!(region.plane.pixels.as_ref(), fixture.indices);
            let expanded: Vec<u8> = region
                .plane
                .pixels
                .as_ref()
                .iter()
                .flat_map(|&index| {
                    let at = usize::from(index) * 4;
                    region.palette.rgba.as_ref()[at..at + 4].iter().copied()
                })
                .collect();
            assert_eq!(expanded, fixture.rgba);
            let source = output.source_packets()[0].source.as_ref().unwrap();
            assert_eq!(source.configuration().original().as_ref(), bytes);
            assert_eq!(source.timing().pts.unwrap().ticks, i128::from(denominator));
            assert_eq!(source.timing().dts, None);
            assert_eq!(source.container_position(), identity(0).container_position);
        }
    }
}

#[test]
fn declared_clock_ratio_and_inferred_dts_do_not_replace_source_facts() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let original = Bytes::copy_from_slice(&budget, &PRIVATE).unwrap();
    let clock = SourceClock::new(2, 2000).unwrap();
    let configuration =
        SourceConfiguration::from_matroska_private(&budget, &original, clock).unwrap();
    assert_eq!(clock.raw_numerator(), 2);
    assert_eq!(clock.raw_denominator(), 2000);
    assert_eq!(clock.time_base().numerator(), 1);
    assert_eq!(clock.time_base().denominator(), 1000);
    let raw = Bytes::copy_from_slice(&budget, COMPLETE).unwrap();
    let used = budget.used();
    let input = configuration
        .packet(
            &budget,
            &raw,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                dts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Inferred,
                }),
                duration: Some(ClockValue {
                    ticks: 2000,
                    origin: ClockOrigin::Source,
                }),
            },
            identity(0),
        )
        .unwrap();
    assert_eq!(
        budget.used(),
        used,
        "binding metadata and byte aliases do not allocate"
    );
    assert_eq!(input.pts, Some(Time::new(1, 1).unwrap()));
    let facts = input.source.as_ref().unwrap();
    assert_eq!(facts.timing().dts.unwrap().origin, ClockOrigin::Inferred);
    assert_eq!(facts.configuration().clock().raw_denominator(), 2000);
    assert_eq!(facts.container_position(), identity(0).container_position);
}

#[test]
fn unknown_and_known_native_sentinel_integer_are_separate_source_values() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let original = Bytes::copy_from_slice(&budget, &PRIVATE).unwrap();
    let configuration = SourceConfiguration::from_matroska_private(
        &budget,
        &original,
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap();
    let raw = Bytes::copy_from_slice(&budget, COMPLETE).unwrap();
    let known = configuration
        .packet(
            &budget,
            &raw,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks: i128::from(i64::MIN),
                    origin: ClockOrigin::Source,
                }),
                ..PacketTiming::default()
            },
            identity(0),
        )
        .unwrap();
    assert_eq!(
        known.pts,
        Some(Time::new(i128::from(i64::MIN), 1000).unwrap())
    );
    let unknown = configuration
        .packet(&budget, &raw, PacketTiming::default(), identity(0))
        .unwrap();
    assert_eq!(unknown.pts, None);
    assert_eq!(unknown.source.as_ref().unwrap().timing().pts, None);
}

#[test]
fn malformed_or_foreign_configuration_rejects_without_taking_ownership() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(1024).unwrap();
    let raw = Bytes::copy_from_slice(&foreign, &PRIVATE).unwrap();
    let clock = SourceClock::new(1, 1000).unwrap();
    assert_eq!(
        SourceConfiguration::from_matroska_private(&budget, &raw, clock).unwrap_err(),
        Error::ForeignPool
    );
    assert_eq!(raw.as_ref(), &PRIVATE[..]);
    let short = Bytes::copy_from_slice(&budget, &PRIVATE[..4]).unwrap();
    assert_eq!(
        SourceConfiguration::from_matroska_private(&budget, &short, clock).unwrap_err(),
        Error::Truncated
    );
    let extra = Bytes::copy_from_slice(&budget, &[0, 1, 0, 2, 0x14, 0]).unwrap();
    assert_eq!(
        SourceConfiguration::from_matroska_private(&budget, &extra, clock).unwrap_err(),
        Error::Invalid("DVB source private length")
    );
    let descriptor = Bytes::copy_from_slice(&budget, &DESCRIPTOR).unwrap();
    assert_eq!(
        SourceConfiguration::from_subtitling_descriptor(
            &budget,
            &descriptor,
            1,
            clock,
            PacketEnvelope::Segments
        )
        .unwrap_err(),
        Error::Invalid("DVB descriptor service index")
    );
    assert!(SourceClock::new(0, 1).is_err());
    assert!(SourceClock::new(1, 0).is_err());
}

#[test]
fn configured_decoder_rejects_mutated_envelope_time_or_missing_facts_before_acceptance() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let raw = Bytes::copy_from_slice(&budget, &PRIVATE).unwrap();
    let configuration = SourceConfiguration::from_matroska_private(
        &budget,
        &raw,
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap();
    let input = configured_packet(&budget, &configuration, 1000);
    let mut decoder = Decoder::new_configured(
        &budget,
        &configuration,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let mut changed = input.clone();
    changed.envelope = PacketEnvelope::PesDataField;
    assert_eq!(
        decoder.provide(&changed).unwrap_err(),
        Error::Invalid("DVB configured packet envelope")
    );
    changed = input.clone();
    changed.pts = None;
    assert_eq!(
        decoder.provide(&changed).unwrap_err(),
        Error::Invalid("DVB configured packet PTS")
    );
    changed = input.clone();
    changed.source = None;
    assert_eq!(
        decoder.provide(&changed).unwrap_err(),
        Error::Invalid("DVB configured packet source facts absent")
    );
    decoder.provide(&input).unwrap();
    assert_eq!(event(&mut decoder, &cancel).pts, input.pts);
}

#[test]
fn retained_event_keeps_original_configuration_after_decoder_and_input_drop() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let initial_charge = budget.used();
    let retained_private = {
        let cancel = AtomicBool::new(false);
        let raw = Bytes::copy_from_slice(&budget, &PRIVATE).unwrap();
        let configuration = SourceConfiguration::from_matroska_private(
            &budget,
            &raw,
            SourceClock::new(1, 1000).unwrap(),
        )
        .unwrap();
        let input = configured_packet(&budget, &configuration, 1000);
        let mut decoder = Decoder::new_configured(
            &budget,
            &configuration,
            GENERATION,
            DisplayLimits::default(),
            &cancel,
        )
        .unwrap();
        decoder.provide(&input).unwrap();
        let output = event(&mut decoder, &cancel);
        drop(decoder);
        drop(input);
        drop(configuration);
        drop(raw);
        assert_eq!(
            output.source_packets()[0]
                .source
                .as_ref()
                .unwrap()
                .configuration()
                .original()
                .as_ref(),
            &PRIVATE[..]
        );
        output.source_packets()[0]
            .source
            .as_ref()
            .unwrap()
            .configuration()
            .original()
            .clone()
    };
    assert_eq!(retained_private.as_ref(), &PRIVATE[..]);
    assert!(budget.used() > initial_charge);
    drop(retained_private);
    assert_eq!(budget.used(), initial_charge);
}

#[test]
fn changed_boundary_requires_reset_and_retained_old_event_keeps_its_clock() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let private = Bytes::copy_from_slice(&budget, &PRIVATE).unwrap();
    let first_configuration = SourceConfiguration::from_matroska_private(
        &budget,
        &private,
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap();
    let next_configuration = SourceConfiguration::from_matroska_private(
        &budget,
        &private,
        SourceClock::new(1, 90000).unwrap(),
    )
    .unwrap();
    let mut decoder = Decoder::new_configured(
        &budget,
        &first_configuration,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let first = configured_packet(&budget, &first_configuration, 1000);
    decoder.provide(&first).unwrap();
    let retained = event(&mut decoder, &cancel);
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(4096), &cancel)
            .unwrap()
            .status,
        DecodeStatus::NeedPacket
    );
    let payload = Bytes::copy_from_slice(&budget, COMPLETE).unwrap();
    let timing = PacketTiming {
        pts: Some(ClockValue {
            ticks: 90000,
            origin: ClockOrigin::Source,
        }),
        ..PacketTiming::default()
    };
    let next = next_configuration
        .packet(&budget, &payload, timing, identity(1))
        .unwrap();
    assert_eq!(
        decoder.provide(&next).unwrap_err(),
        Error::Invalid("DVB configured source changed without reset")
    );
    assert_eq!(
        decoder
            .reset_with_configuration(&next_configuration, GENERATION, &cancel)
            .unwrap_err(),
        Error::Invalid("DVB reset requires a new generation")
    );
    decoder
        .reset_with_configuration(&next_configuration, GENERATION + 1, &cancel)
        .unwrap();
    let next = next_configuration
        .packet(
            &budget,
            &payload,
            timing,
            PacketIdentity {
                generation: GENERATION + 1,
                ..identity(0)
            },
        )
        .unwrap();
    decoder.provide(&next).unwrap();
    let output = event(&mut decoder, &cancel);
    assert_eq!(retained.pts, output.pts);
    assert_eq!(
        retained.source_packets()[0]
            .source
            .as_ref()
            .unwrap()
            .configuration()
            .clock()
            .raw_denominator(),
        1000
    );
    assert_eq!(
        output.source_packets()[0]
            .source
            .as_ref()
            .unwrap()
            .configuration()
            .clock()
            .raw_denominator(),
        90000
    );
    assert_eq!(retained.generation, GENERATION);
    assert_eq!(output.generation, GENERATION + 1);
}

#[test]
fn descriptor_service_selection_keeps_all_original_entries_and_language_bytes() {
    let budget = Budget::new(4096).unwrap();
    let bytes = [
        0x59, 16, b'e', b'n', b'g', 0x14, 0, 1, 0, 2, b'd', b'e', b'u', 0x10, 0, 3, 0, 4,
    ];
    let raw = Bytes::copy_from_slice(&budget, &bytes).unwrap();
    let configuration = SourceConfiguration::from_subtitling_descriptor(
        &budget,
        &raw,
        1,
        SourceClock::new(1, 90000).unwrap(),
        PacketEnvelope::Segments,
    )
    .unwrap();
    assert_eq!(configuration.original().as_ref(), &bytes[..]);
    assert_eq!(configuration.language_bytes(), Some(&b"deu"[..]));
    assert_eq!(configuration.service().composition_page, 3);
    assert_eq!(configuration.service().ancillary_page, 4);
    assert_eq!(configuration.service_private_view(), [0, 3, 0, 4, 0x10]);
}

#[test]
fn configured_pes_packets_keep_the_declared_envelope_across_a_split_display() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let raw = Bytes::copy_from_slice(&budget, &DESCRIPTOR).unwrap();
    let configuration = SourceConfiguration::from_subtitling_descriptor(
        &budget,
        &raw,
        0,
        SourceClock::new(1, 90000).unwrap(),
        PacketEnvelope::PesDataField,
    )
    .unwrap();
    let mut decoder = Decoder::new_configured(
        &budget,
        &configuration,
        GENERATION,
        DisplayLimits::default(),
        &cancel,
    )
    .unwrap();
    let split: [&[u8]; 2] = [
        include_bytes!("state-fixtures/same-pts-packet-split.0.dvb"),
        include_bytes!("state-fixtures/same-pts-packet-split.1.dvb"),
    ];
    for (sequence, segments) in split.iter().enumerate() {
        let mut bytes = vec![0x20, 0];
        bytes.extend_from_slice(segments);
        bytes.push(0xff);
        let payload = Bytes::copy_from_slice(&budget, &bytes).unwrap();
        let input = configuration
            .packet(
                &budget,
                &payload,
                PacketTiming {
                    pts: Some(ClockValue {
                        ticks: 90000,
                        origin: ClockOrigin::Source,
                    }),
                    ..PacketTiming::default()
                },
                identity(u64::try_from(sequence).unwrap()),
            )
            .unwrap();
        decoder.provide(&input).unwrap();
        if sequence == 0 {
            let mut paused = false;
            for _ in 0..100_000 {
                let mut work = WorkBudget::new(1);
                let next = decoder.step(&mut work, &cancel).unwrap();
                match next.status {
                    DecodeStatus::NeedPacket => {
                        paused = true;
                        break;
                    }
                    DecodeStatus::Yield => assert!(work.spent() > 0),
                    status => panic!("PES marker created an event/end: {status:?}"),
                }
            }
            assert!(paused);
        }
    }
    let output = event(&mut decoder, &cancel);
    assert_eq!(output.source_packets().len(), 2);
    for source in output.source_packets() {
        assert_eq!(source.envelope, PacketEnvelope::PesDataField);
        assert_eq!(
            source.source.as_ref().unwrap().configuration().envelope(),
            PacketEnvelope::PesDataField
        );
        assert_eq!(&source.payload.as_ref()[..2], &[0x20, 0]);
        assert_eq!(source.payload.as_ref().last(), Some(&0xff));
        assert_eq!(
            source.source.as_ref().unwrap().timing().pts.unwrap().ticks,
            90000
        );
    }
}
