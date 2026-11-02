//! Primary EN300743 inactive-fill regression; source art predates any proposed fix.
//! Two background-change cases violate5.1.5 and test receiver tolerance only.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, ColourSource, DecodeStatus, Decoder, DisplayEvent, DisplayLimits,
    PacketIdentity, PacketTiming, SourceClock, SourceConfiguration,
};

const INITIAL_CDS: &[u8] = include_bytes!("rcs-fill-fixtures/initial.cds");
const REPLACEMENT_CDS: &[u8] = include_bytes!("rcs-fill-fixtures/replacement.cds");
const GENERATION: u64 = 9;

fn decode(pool: &Budget, inputs: &[&[u8]], units: u64) -> Result<Vec<DisplayEvent>, Error> {
    let cancel = AtomicBool::new(false);
    let private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x14])?;
    let config =
        SourceConfiguration::from_matroska_private(pool, &private, SourceClock::new(1, 90000)?)?;
    let mut decoder =
        Decoder::new_configured(pool, &config, GENERATION, DisplayLimits::default(), &cancel)?;
    let mut next = 0;
    let mut ending = false;
    let mut events = Vec::new();
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(units);
        let progress = decoder.step(&mut work, &cancel)?;
        match progress.status {
            DecodeStatus::Yield => assert!(work.spent() > 0),
            DecodeStatus::NeedPacket => {
                if let Some(bytes) = inputs.get(next) {
                    let raw = Bytes::copy_from_slice(pool, bytes)?;
                    let sequence = u64::try_from(next).unwrap();
                    let packet = config.packet(
                        pool,
                        &raw,
                        PacketTiming {
                            pts: Some(ClockValue {
                                ticks: (i128::from(sequence) + 1) * 90000,
                                origin: ClockOrigin::Source,
                            }),
                            dts: None,
                            duration: None,
                        },
                        PacketIdentity {
                            generation: GENERATION,
                            sequence,
                            container_position: None,
                        },
                    )?;
                    decoder.provide(&packet)?;
                    next += 1;
                } else {
                    assert!(!ending, "repeated source request after logical finish");
                    decoder.finish_input()?;
                    ending = true;
                }
            }
            DecodeStatus::Event => events.push(progress.event.unwrap()),
            DecodeStatus::NeedOutput => return Err(progress.pressure.unwrap()),
            DecodeStatus::Finished => {
                assert!(ending);
                assert_eq!(next, inputs.len());
                return Ok(events);
            }
        }
    }
    panic!("tiny two-display fixture exceeded bounded poll cap");
}

struct SourceCase<'a> {
    initial: &'a [u8],
    update: &'a [u8],
    rcs: &'a [u8],
    indices: &'a [u8],
    rgba: &'a [u8],
    old_fill: u8,
    palette_update: bool,
}

fn check_pair(case: SourceCase<'_>) {
    let SourceCase {
        initial,
        update,
        rcs,
        indices,
        rgba,
        old_fill,
        palette_update,
    } = case;
    for units in [1, 7, 4096] {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        // decode() drops the decoder and all caller-owned input/config aliases.
        let events = decode(&pool, &[initial, update], units).unwrap_or_else(|error| {
            panic!("receiver compatibility input rejected, units={units}: {error:?}")
        });
        assert_eq!(events.len(), 2);
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event.generation, GENERATION);
            assert_eq!(event.sequence, u64::try_from(sequence).unwrap());
            assert_eq!(event.pts, Some(Time::new(sequence as i128 + 1, 1).unwrap()));
            assert_eq!(event.regions().len(), 1);
            assert_eq!(event.source_packets().len(), 1);
            assert_eq!(event.source_packets()[0].sequence, sequence as u64);
            assert_eq!(
                event.source_packets()[0].payload.as_ref(),
                [initial, update][sequence]
            );
            assert_eq!(
                event.source_packets()[0]
                    .source
                    .as_ref()
                    .unwrap()
                    .configuration()
                    .original()
                    .as_ref(),
                [0, 1, 0, 2, 0x14]
            );
        }
        let before = &events[0].regions()[0];
        let after = &events[1].regions()[0];
        assert_eq!(before.plane.pixels.as_ref(), [old_fill; 8]);
        assert_eq!(after.original_composition.as_ref(), rcs);
        assert_eq!(after.plane.pixels.as_ref(), indices);
        let actual_rgba: Vec<u8> = indices
            .iter()
            .flat_map(|&index| {
                let at = usize::from(index) * 4;
                after.palette.rgba.as_ref()[at..at + 4].iter().copied()
            })
            .collect();
        assert_eq!(actual_rgba, rgba);
        if palette_update {
            let old = &before.palette.entries()[1];
            let changed = &after.palette.entries()[1];
            assert_eq!(old.original_segment.as_ref().unwrap().as_ref(), INITIAL_CDS);
            assert_eq!(
                changed.original_segment.as_ref().unwrap().as_ref(),
                REPLACEMENT_CDS
            );
            assert_eq!(
                changed.source,
                ColourSource::YCrCbT {
                    y: 16,
                    cr: 128,
                    cb: 128,
                    transparency: 128,
                    full_resolution: true,
                }
            );
            assert_eq!(changed.source.opacity().unwrap(), (128, 256));
            assert_eq!(&before.palette.rgba.as_ref()[4..8], &[255, 255, 255, 255]);
            assert_eq!(&after.palette.rgba.as_ref()[4..8], &[0, 0, 0, 128]);
            assert_eq!(
                &after.palette.studio_unrepaired_rgba.as_ref()[4..8],
                &[0, 0, 0, 127]
            );
            for index in [0, 2, 3] {
                assert_eq!(
                    after.palette.entries()[index]
                        .original_segment
                        .as_ref()
                        .unwrap()
                        .as_ref(),
                    INITIAL_CDS
                );
                assert_eq!(
                    after.palette.entries()[index].source,
                    before.palette.entries()[index].source
                );
            }
        }
        assert!(
            pool.used() > baseline,
            "retained events own charged source/output"
        );
        drop(events);
        assert_eq!(pool.used(), baseline, "all retained aliases released");
    }
}

macro_rules! source_case {
    ($name:ident, $stem:literal, $fill:literal, $palette:literal) => {
        #[test]
        fn $name() {
            check_pair(SourceCase {
                initial: include_bytes!(concat!("rcs-fill-fixtures/", $stem, ".0.dvb")),
                update: include_bytes!(concat!("rcs-fill-fixtures/", $stem, ".1.dvb")),
                rcs: include_bytes!(concat!("rcs-fill-fixtures/", $stem, ".1.rcs")),
                indices: include_bytes!(concat!("rcs-fill-fixtures/", $stem, ".1.indices")),
                rgba: include_bytes!(concat!("rcs-fill-fixtures/", $stem, ".1.rgba")),
                old_fill: $fill,
                palette_update: $palette,
            });
        }
    };
}

source_case!(
    inactive_background_change_accepts_current_object_update,
    "object-ignored-background",
    2,
    false
);
source_case!(
    inactive_background_change_accepts_partial_clut_update,
    "palette-ignored-background",
    1,
    true
);
source_case!(
    unchanged_background_control_keeps_previous_pixels,
    "object-unchanged-background-control",
    2,
    false
);
source_case!(
    active_fill_control_uses_new_background,
    "fill-changed-background-control",
    2,
    false
);
