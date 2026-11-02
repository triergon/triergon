//! Complete-DVB source regressions; native discrepancies must not rewrite art.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, ColourSource, DecodeStatus, Decoder, Depth, DisplayEvent,
    DisplayLimits, PacketIdentity, PacketTiming, SourceClock, SourceConfiguration,
};

struct SourceCase<'a> {
    name: &'a str,
    packet: &'a [u8],
    indices: &'a [u8],
    rgba: &'a [u8],
    palette: &'a [u8],
    cds: &'a [u8],
    ods: &'a [u8],
    depth: Depth,
    nonmodifying: bool,
}

fn decode(pool: &Budget, packet: &[u8], units: u64) -> Result<DisplayEvent, Error> {
    let cancel = AtomicBool::new(false);
    let raw_private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x14])?;
    let config = SourceConfiguration::from_matroska_private(
        pool,
        &raw_private,
        SourceClock::new(1000000, 1000000000)?,
    )?;
    let source = config.packet(
        pool,
        &Bytes::copy_from_slice(pool, packet)?,
        PacketTiming {
            pts: Some(ClockValue {
                ticks: 1000,
                origin: ClockOrigin::Source,
            }),
            dts: None,
            duration: None,
        },
        PacketIdentity {
            generation: 11,
            sequence: 0,
            container_position: None,
        },
    )?;
    let mut decoder =
        Decoder::new_configured(pool, &config, 11, DisplayLimits::default(), &cancel)?;
    decoder.provide(&source)?;
    decoder.finish_input()?;
    let mut event = None;
    for _ in 0..1000000 {
        let mut work = WorkBudget::new(units);
        let progress = decoder.step(&mut work, &cancel)?;
        match progress.status {
            DecodeStatus::Yield => assert!(work.spent() > 0),
            DecodeStatus::Event => {
                assert!(event.is_none());
                event = progress.event;
            }
            DecodeStatus::NeedOutput => return Err(progress.pressure.unwrap()),
            DecodeStatus::NeedPacket => panic!("whole supplied source unexpectedly starved"),
            DecodeStatus::Finished => return Ok(event.expect("one complete source display")),
        }
    }
    panic!("tiny source exceeded bounded poll policy")
}

fn check_source(source: SourceCase<'_>) {
    for units in [1, 7, 4096] {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        let event = decode(&pool, source.packet, units)
            .unwrap_or_else(|error| panic!("{} units={units}: {error:?}", source.name));
        assert_eq!(event.pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(event.timeout_seconds, 2);
        assert_eq!(event.source_packets().len(), 1);
        assert_eq!(event.source_packets()[0].payload.as_ref(), source.packet);
        let facts = event.source_packets()[0].source.as_ref().unwrap();
        assert_eq!(
            facts.configuration().original().as_ref(),
            [0, 1, 0, 2, 0x14]
        );
        assert_eq!(facts.configuration().clock().raw_numerator(), 1000000);
        assert_eq!(facts.configuration().clock().raw_denominator(), 1000000000);
        assert_eq!(
            facts.timing().pts,
            Some(ClockValue {
                ticks: 1000,
                origin: ClockOrigin::Source
            })
        );
        assert_eq!(event.regions().len(), 1);
        let region = &event.regions()[0];
        assert_eq!(region.plane.depth, source.depth);
        assert_eq!(
            (
                region.position.x,
                region.position.y,
                region.size.width,
                region.size.height
            ),
            (1, 1, 5, 2)
        );
        assert_eq!(region.plane.pixels.as_ref(), source.indices);
        assert_eq!(region.palette.rgba.as_ref(), source.palette);
        assert_eq!(
            region.plane.source.object.as_ref().unwrap().as_ref(),
            source.ods
        );
        assert_eq!(source.ods[8] & 2 != 0, source.nonmodifying);
        for (index, cell) in region.palette.entries().iter().enumerate() {
            assert_eq!(cell.original_segment.as_ref().unwrap().as_ref(), source.cds);
            assert_eq!(
                cell.source,
                ColourSource::YCrCbT {
                    y: if index == 0 {
                        0
                    } else if index == 1 || index == 2 {
                        235
                    } else {
                        16
                    },
                    cr: if index == 0 { 0 } else { 128 },
                    cb: if index == 0 { 0 } else { 128 },
                    transparency: 0,
                    full_resolution: true,
                }
            );
        }
        let pixels: Vec<u8> = region
            .plane
            .pixels
            .as_ref()
            .iter()
            .flat_map(|index| {
                let at = usize::from(*index) * 4;
                region.palette.rgba.as_ref()[at..at + 4].iter().copied()
            })
            .collect();
        assert_eq!(pixels, source.rgba);
        assert!(pool.used() > baseline);
        drop(event);
        assert_eq!(pool.used(), baseline);
    }
}

macro_rules! source_case {
    ($function:ident, $name:literal, $depth:ident, $flag:literal) => {
        #[test]
        fn $function() {
            check_source(SourceCase {
                name: $name,
                packet: include_bytes!(concat!("nonmodifying-fixtures/", $name, ".dvb")),
                indices: include_bytes!(concat!("nonmodifying-fixtures/", $name, ".indices")),
                rgba: include_bytes!(concat!("nonmodifying-fixtures/", $name, ".rgba")),
                palette: include_bytes!(concat!("nonmodifying-fixtures/", $name, ".palette")),
                cds: include_bytes!(concat!("nonmodifying-fixtures/", $name, ".cds")),
                ods: include_bytes!(concat!("nonmodifying-fixtures/", $name, ".ods")),
                depth: Depth::$depth,
                nonmodifying: $flag,
            });
        }
    };
}

// Original complete-source cases; all art is defined before native/Rust runs.
source_case!(dvb2_literal_flag0, "dvb2-literal-flag0", Two, false);
source_case!(dvb2_literal_flag1, "dvb2-literal-flag1", Two, true);
source_case!(dvb2_run_flag0, "dvb2-run-flag0", Two, false);
source_case!(dvb2_run_flag1, "dvb2-run-flag1", Two, true);
source_case!(dvb4_literal_flag0, "dvb4-literal-flag0", Four, false);
source_case!(dvb4_literal_flag1, "dvb4-literal-flag1", Four, true);
source_case!(dvb4_run_flag0, "dvb4-run-flag0", Four, false);
source_case!(dvb4_run_flag1, "dvb4-run-flag1", Four, true);
source_case!(dvb8_literal_flag0, "dvb8-literal-flag0", Eight, false);
source_case!(dvb8_literal_flag1, "dvb8-literal-flag1", Eight, true);
source_case!(dvb8_run_flag0, "dvb8-run-flag0", Eight, false);
source_case!(dvb8_run_flag1, "dvb8-run-flag1", Eight, true);
