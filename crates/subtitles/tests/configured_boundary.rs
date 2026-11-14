//! Configured-boundary tests.
#![forbid(unsafe_code)]

use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, Time};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, PacketEnvelope, PacketIdentity, PacketTiming, SourceClock,
    SourceConfiguration,
};

const GENERATION: u64 = 7;
const PRIVATE: [u8; 5] = [0, 1, 0, 2, 0x14];
const DESCRIPTOR: [u8; 10] = [0x59, 8, b'e', b'n', b'g', 0x14, 0, 1, 0, 2];
const COMPLETE: &[u8] = include_bytes!("boundary-fixtures/dvb2-literals-fields.dvb");

struct Fixture {
    bytes: &'static [u8],
    subtype: u8,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        bytes: COMPLETE,
        subtype: 0x14,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-ancillary-pages.dvb"),
        subtype: 0x14,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-default-clut2.dvb"),
        subtype: 0x10,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-nonmodifying.dvb"),
        subtype: 0x14,
    },
    Fixture {
        bytes: include_bytes!("boundary-fixtures/dvb-partial-alpha-projection.dvb"),
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

#[test]
fn source_private_descriptor_and_clock_are_explicit_and_preserved() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
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
            let source = packet.source.as_ref().unwrap();
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
