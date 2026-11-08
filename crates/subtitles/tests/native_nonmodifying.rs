#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, PacketIdentity,
    PacketTiming, SourceClock, SourceConfiguration,
};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    source_indices: &'static [u8],
    source_palette: &'static [u8],
    native_indices: &'static [u8],
    native_palette: &'static [u8],
    receiver_words: &'static [u32],
    non_modifying: bool,
    depth: u8,
    pts_raw: i64,
}
#[path = "native-nonmodifying-support/cases.rs"]
mod cases;
struct Observed {
    events: Vec<DisplayEvent>,
    error: Option<Error>,
}
fn decode(pool: &Budget, bytes: &[u8], quantum: u64) -> Observed {
    let private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x14]).unwrap();
    let config = SourceConfiguration::from_matroska_private(
        pool,
        &private,
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder =
        Decoder::new_configured(pool, &config, 13, DisplayLimits::default(), &cancel).unwrap();
    let raw = Bytes::copy_from_slice(pool, bytes).unwrap();
    let packet = config
        .packet(
            pool,
            &raw,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                dts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                duration: Some(ClockValue {
                    ticks: 2000,
                    origin: ClockOrigin::Source,
                }),
            },
            PacketIdentity {
                generation: 13,
                sequence: 0,
                container_position: None,
            },
        )
        .unwrap();
    decoder.provide(&packet).unwrap();
    let mut observed = Observed {
        events: Vec::new(),
        error: None,
    };
    let mut ended = false;
    for _ in 0..1_000_000 {
        let progress = match decoder.step(&mut WorkBudget::new(quantum), &cancel) {
            Ok(progress) => progress,
            Err(error) => {
                observed.error = Some(error);
                return observed;
            }
        };
        match progress.status {
            DecodeStatus::Yield => {}
            DecodeStatus::Event => observed.events.push(progress.event.unwrap()),
            DecodeStatus::NeedPacket => {
                assert!(!ended);
                decoder.finish_input().unwrap();
                ended = true;
            }
            DecodeStatus::NeedOutput => panic!("small full-CLUT input exhausted4MiB"),
            DecodeStatus::Finished => return observed,
        }
    }
    panic!("bounded input exceeded poll cap")
}
/// The receiver decoder against the recorded output of the reference
/// decoder: the subtitle clock, rectangle, index plane and palette.
fn check_native(index: usize) {
    let case = &cases::CASES[index];
    let pool = Budget::new(4 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &[0, 1, 0, 2, 0x14], Options::default());
    let mut work = WorkBudget::new(u64::MAX);
    let subtitle = decoder
        .decode(case.payload, Some(case.pts_raw), &mut work, &cancel)
        .unwrap()
        .expect("presented display set");
    assert_eq!(subtitle.pts, Some(case.pts_raw));
    assert_eq!(
        (subtitle.start_display_ms, subtitle.end_display_ms),
        (0, 2000)
    );
    let rects: Vec<_> = subtitle.rects().collect();
    assert_eq!(rects.len(), 1);
    let rect = &rects[0];
    assert_eq!(
        [
            rect.x,
            rect.y,
            i32::from(rect.width),
            i32::from(rect.height)
        ],
        [1, 1, 5, 2]
    );
    assert_eq!(rect.palette, case.receiver_words, "{}", case.id);
    assert_eq!(rect.palette.len(), 1 << case.depth);
    let rgba: Vec<u8> = rect
        .palette
        .iter()
        .flat_map(|w| [(w >> 16) as u8, (w >> 8) as u8, *w as u8, (w >> 24) as u8])
        .collect();
    assert_eq!(rgba, case.native_palette, "{}", case.id);
    assert_eq!(
        rect.pixels, case.native_indices,
        "{} flag={}",
        case.id, case.non_modifying
    );
}

include!("native-nonmodifying-support/names.rs");

#[test]
fn original_twelve_source_planes_and_palettes_are_unchanged() {
    for case in cases::CASES {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let observed = decode(&pool, case.payload, quantum);
            assert_eq!(observed.error, None);
            assert_eq!(observed.events.len(), 1);
            let event = &observed.events[0];
            assert_eq!(event.source_packets()[0].payload.as_ref(), case.payload);
            let region = &event.regions()[0];
            assert_eq!(region.plane.pixels.as_ref(), case.source_indices);
            assert_eq!(region.palette.rgba.as_ref(), case.source_palette);
            drop(observed);
            assert_eq!(pool.used(), baseline);
        }
    }
}
#[test]
fn source_mapping_rule_and_all_original_map_bytes_are_preserved() {
    for (input, indices) in [
        (
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to4-raw1-to2-raw2-to1.dvb"
            )[..],
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to4-raw1-to2-raw2-to1.source-indices"
            )[..],
        ),
        (
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to8-raw1-to2-raw2-to1.dvb"
            )[..],
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to8-raw1-to2-raw2-to1.source-indices"
            )[..],
        ),
    ] {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let observed = decode(&pool, input, quantum);
            assert_eq!(observed.error, None);
            assert_eq!(observed.events.len(), 1);
            assert_eq!(
                observed.events[0].source_packets()[0].payload.as_ref(),
                input
            );
            assert_eq!(
                observed.events[0].regions()[0].plane.pixels.as_ref(),
                indices
            );
            drop(observed);
            assert_eq!(pool.used(), baseline);
        }
    }
}
#[test]
fn source_rejects_a_logical_row_overrun() {
    {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let observed = decode(
                &pool,
                include_bytes!("native-nonmodifying-fixtures/logical-width-overrun.dvb"),
                quantum,
            );
            assert_eq!(
                observed.error,
                Some(Error::Invalid("DVB run exceeds source row"))
            );
            assert!(observed.events.is_empty());
            drop(observed);
            assert_eq!(pool.used(), baseline);
        }
    }
}
