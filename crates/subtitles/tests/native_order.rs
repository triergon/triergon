#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, PacketIdentity,
    PacketTiming, SourceClock, SourceConfiguration,
};

struct ExpectedRect {
    source_id: u8,
    geometry: [u32; 4],
    stride: usize,
    indices: &'static [u8],
    rgba: &'static [u8],
    words: &'static [u32],
}
struct Case {
    id: &'static str,
    payload: &'static [u8],
    original_pcs: &'static [u8],
    source_ids: &'static [u8],
    conforming: bool,
    pts_raw: i64,
    start_ms: u32,
    end_ms: u32,
    rects: &'static [ExpectedRect],
}
#[path = "native-order-fixtures/cases.rs"]
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
            DecodeStatus::NeedOutput => panic!("small frozen source unexpectedly exhausted4MiB"),
            DecodeStatus::Finished => return observed,
        }
    }
    panic!("bounded source did not terminate")
}
/// The receiver decoder against the recorded output of the reference
/// decoder: rectangle order and geometry, index planes, palettes and clock.
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
        (case.start_ms, case.end_ms)
    );
    let rects: Vec<_> = subtitle.rects().collect();
    assert_eq!(rects.len(), case.rects.len(), "{}", case.id);
    for (rect, expected) in rects.iter().zip(case.rects) {
        let geometry = [
            rect.x,
            rect.y,
            i32::from(rect.width),
            i32::from(rect.height),
        ];
        assert_eq!(geometry.map(|v| v as u32), expected.geometry, "{}", case.id);
        assert_eq!(usize::from(rect.width), expected.stride);
        assert_eq!(rect.pixels, expected.indices);
        assert_eq!(rect.palette, expected.words);
        let rgba: Vec<u8> = rect
            .palette
            .iter()
            .flat_map(|w| [(w >> 16) as u8, (w >> 8) as u8, *w as u8, (w >> 24) as u8])
            .collect();
        assert_eq!(rgba, expected.rgba);
    }
}
fn check_source(index: usize) {
    let case = &cases::CASES[index];
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let result = decode(&pool, case.payload, quantum);
        if case.conforming {
            assert_eq!(result.error, None);
            assert_eq!(result.events.len(), 1);
            let event = &result.events[0];
            assert_eq!(
                event.page_composition.as_ref().unwrap().as_ref(),
                case.original_pcs
            );
            assert_eq!(
                event
                    .regions()
                    .iter()
                    .map(|region| region.id)
                    .collect::<Vec<_>>(),
                case.source_ids
            );
            // The receiver presents the same regions in reverse.
            assert!(
                case.rects
                    .iter()
                    .map(|r| r.source_id)
                    .eq(case.source_ids.iter().rev().copied())
            );
            for region in event.regions() {
                assert_eq!(&region.palette.rgba.as_ref()[..4], &[0, 0, 0, 0]);
            }
        } else {
            assert_eq!(
                result.error,
                Some(Error::Invalid("DVB visible region order"))
            );
            assert!(result.events.is_empty());
        }
        drop(result);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn native_original_is_reverse_pcs() {
    check_native(0);
}
#[test]
fn native_rcs_arrival_swap_does_not_change_reverse_pcs() {
    check_native(1);
}
#[test]
fn native_renamed_ids_do_not_sort_the_output() {
    check_native(2);
}
#[test]
fn native_nonascending_pcs_tolerance_keeps_literal_reverse() {
    check_native(3);
}
#[test]
fn source_original_order_is_unchanged() {
    check_source(0);
}
#[test]
fn source_rcs_arrival_swap_is_unchanged() {
    check_source(1);
}
#[test]
fn source_renamed_ids_are_unchanged() {
    check_source(2);
}
#[test]
fn source_nonascending_pcs_remains_rejected() {
    check_source(3);
}

#[test]
fn source_rejects_regions_sharing_scan_lines() {
    {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let result = decode(
                &pool,
                include_bytes!("native-order-fixtures/input/overlap-invalid.dvb"),
                quantum,
            );
            assert_eq!(
                result.error,
                Some(Error::Invalid("DVB visible regions share scan lines"))
            );
            assert!(result.events.is_empty());
            drop(result);
            assert_eq!(pool.used(), baseline);
        }
    }
}
#[test]
fn source_window_bounds_fail_without_partial_event() {
    {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let result = decode(
                &pool,
                include_bytes!("native-order-fixtures/input/window-invalid.dvb"),
                quantum,
            );
            assert_eq!(
                result.error,
                Some(Error::Invalid("DVB region outside display window"))
            );
            assert!(result.events.is_empty());
            drop(result);
            assert_eq!(pool.used(), baseline);
        }
    }
}

#[test]
fn source_empty_regions_still_validate_display_bounds() {
    // PCS + a fresh filled 2x2 RCS + EODS, without ODS or a display definition.
    // The default 720x576 canvas applies even when no rectangle is published.
    const EMPTY_REGION: &[u8] = &[
        0x0f, 0x10, 0, 1, 0, 8, 2, 0x0b, 1, 0xff, 0, 0, 0, 0, 0x0f, 0x11, 0, 1, 0, 10, 1, 0x0f, 0,
        2, 0, 2, 0x27, 0, 0, 3, 0x0f, 0x80, 0, 1, 0, 0,
    ];
    for (x, y) in [(719_u16, 0_u16), (0, 575)] {
        let mut payload = EMPTY_REGION.to_vec();
        payload[10..12].copy_from_slice(&x.to_be_bytes());
        payload[12..14].copy_from_slice(&y.to_be_bytes());
        {
            for quantum in [1, 7, 4096] {
                let pool = Budget::new(4 << 20).unwrap();
                let baseline = pool.used();
                let result = decode(&pool, &payload, quantum);
                assert_eq!(
                    result.error,
                    Some(Error::Invalid("DVB region outside display window")),
                    "work={quantum} position=({x},{y})"
                );
                assert!(result.events.is_empty());
                drop(result);
                assert_eq!(pool.used(), baseline);
            }
        }
    }
}
