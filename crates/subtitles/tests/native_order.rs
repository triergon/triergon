#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};

struct ExpectedRect {
    geometry: [u32; 4],
    stride: usize,
    indices: &'static [u8],
    rgba: &'static [u8],
    words: &'static [u32],
}
struct Case {
    id: &'static str,
    payload: &'static [u8],
    pts_raw: i64,
    start_ms: u32,
    end_ms: u32,
    rects: &'static [ExpectedRect],
}
#[path = "native-order-fixtures/cases.rs"]
mod cases;

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
        .subtitle()
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
