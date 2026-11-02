//! Specific7.2.6 EOD rule, with source legality separate from receiver tolerance.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, PacketEnvelope,
    PacketIdentity, PacketTiming, SourceClock, SourceConfiguration,
};

const PRIVATE: [u8; 5] = [0, 1, 0, 2, 0x14];
const DESCRIPTOR: [u8; 10] = [0x59, 8, b'e', b'n', b'g', 0x14, 0, 1, 0, 2];
const INDICES: &[u8] = include_bytes!("ancillary-eod-fixtures/source.indices");
const RGBA: &[u8] = include_bytes!("ancillary-eod-fixtures/source.rgba");

struct SourceCase<'a> {
    name: &'a str,
    packets: &'a [&'a [u8]],
    pes: bool,
    error: Option<Error>,
}

struct Observation {
    events: Vec<DisplayEvent>,
    error: Option<Error>,
    finished: bool,
}

fn decode(pool: &Budget, source: &SourceCase<'_>, units: u64) -> Observation {
    let cancel = AtomicBool::new(false);
    let config = if source.pes {
        SourceConfiguration::from_subtitling_descriptor(
            pool,
            &Bytes::copy_from_slice(pool, &DESCRIPTOR).unwrap(),
            0,
            SourceClock::new(1, 90000).unwrap(),
            PacketEnvelope::PesDataField,
        )
        .unwrap()
    } else {
        SourceConfiguration::from_matroska_private(
            pool,
            &Bytes::copy_from_slice(pool, &PRIVATE).unwrap(),
            SourceClock::new(1000000, 1000000000).unwrap(),
        )
        .unwrap()
    };
    let ticks = if source.pes { 90000 } else { 1000 };
    let mut decoder =
        Decoder::new_configured(pool, &config, 13, DisplayLimits::default(), &cancel).unwrap();
    let mut result = Observation {
        events: Vec::new(),
        error: None,
        finished: false,
    };
    let mut next = 0;
    let mut ending = false;
    for _ in 0..1000000 {
        let mut work = WorkBudget::new(units);
        let progress = match decoder.step(&mut work, &cancel) {
            Ok(progress) => progress,
            Err(error) => {
                result.error = Some(error);
                return result;
            }
        };
        match progress.status {
            DecodeStatus::Yield => assert!(work.spent() > 0),
            DecodeStatus::NeedPacket => {
                if let Some(bytes) = source.packets.get(next) {
                    let raw = Bytes::copy_from_slice(pool, bytes).unwrap();
                    let packet = config
                        .packet(
                            pool,
                            &raw,
                            PacketTiming {
                                pts: Some(ClockValue {
                                    ticks,
                                    origin: ClockOrigin::Source,
                                }),
                                dts: None,
                                duration: None,
                            },
                            PacketIdentity {
                                generation: 13,
                                sequence: next as u64,
                                container_position: None,
                            },
                        )
                        .unwrap();
                    decoder.provide(&packet).unwrap();
                    next += 1;
                } else {
                    assert!(!ending);
                    decoder.finish_input().unwrap();
                    ending = true;
                }
            }
            DecodeStatus::Event => result.events.push(progress.event.unwrap()),
            DecodeStatus::NeedOutput => panic!("small source unexpectedly exceeded test pool"),
            DecodeStatus::Finished => {
                result.finished = true;
                return result;
            }
        }
    }
    panic!("bounded source poll cap exceeded")
}

fn check_source(source: SourceCase<'_>) {
    for units in [1, 7, 4096] {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        let observed = decode(&pool, &source, units);
        assert_eq!(
            observed.error, source.error,
            "{} units={units}",
            source.name
        );
        assert_eq!(observed.finished, source.error.is_none());
        assert_eq!(observed.events.len(), usize::from(source.error.is_none()));
        for event in &observed.events {
            assert_eq!(event.pts, Some(Time::new(1, 1).unwrap()));
            assert_eq!(event.source_packets().len(), source.packets.len());
            for (packet, original) in event.source_packets().iter().zip(source.packets) {
                assert_eq!(packet.payload.as_ref(), *original);
            }
            assert_eq!(event.regions().len(), 1);
            let region = &event.regions()[0];
            assert_eq!(region.plane.pixels.as_ref(), INDICES);
            let rgba: Vec<u8> = region
                .plane
                .pixels
                .as_ref()
                .iter()
                .flat_map(|index| {
                    let at = usize::from(*index) * 4;
                    region.palette.rgba.as_ref()[at..at + 4].iter().copied()
                })
                .collect();
            assert_eq!(rgba, RGBA);
        }
        drop(observed);
        assert_eq!(pool.used(), baseline);
    }
}

macro_rules! raw_case {
    ($name:ident, $stem:literal, $error:expr) => {
        #[test]
        fn $name() {
            check_source(SourceCase {
                name: $stem,
                packets: &[include_bytes!(concat!(
                    "ancillary-eod-fixtures/",
                    $stem,
                    ".0.dvb"
                ))],
                pes: false,
                error: $error,
            });
        }
    };
}

raw_case!(
    shared_ancillary_eod_completes_selected_display,
    "shared-ancillary-end",
    None
);
raw_case!(
    nonempty_ancillary_eod_is_rejected,
    "ancillary-end-nonempty",
    Some(Error::Invalid("DVB EOD payload"))
);
raw_case!(
    composition_eod_without_shared_data_control,
    "composition-end-control",
    None
);
raw_case!(
    shared_composition_eod_receiver_tolerance_control,
    "shared-composition-end-compat",
    None
);
raw_case!(
    foreign_service_eod_does_not_finish_selected_service,
    "foreign-end-filter-control",
    None
);

#[test]
fn shared_ancillary_eod_completes_same_pts_split_pes() {
    check_source(SourceCase {
        name: "shared-split-pes-end",
        packets: &[
            include_bytes!("ancillary-eod-fixtures/shared-split-pes-end.0.dvb"),
            include_bytes!("ancillary-eod-fixtures/shared-split-pes-end.1.dvb"),
        ],
        pes: true,
        error: None,
    });
}
