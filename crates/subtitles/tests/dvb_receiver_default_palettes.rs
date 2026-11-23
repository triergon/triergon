#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{Decoder, Options};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    indices: &'static [u8],
    native_rgba: &'static [u8],
    receiver_words: &'static [u32],
    rect: [u32; 4],
    pts_microseconds: i64,
}

#[path = "native-defaults-support/cases.rs"]
mod cases;

// The expected components are EN 300 743 Tables 36 to 38 tabulated by hand,
// rounded to nearest; no library colour routine computes them.

/// The receiver decoder against the recorded output of the reference
/// decoder: the subtitle clock, rectangle, index plane and ARGB words.
fn check(at: usize) {
    let case = &cases::CASES[at];
    let pool = Budget::new(4 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new(&pool, &[0, 1, 0, 2, 0x10], Options::default());
    let mut work = WorkBudget::new(u64::MAX);
    let subtitle = decoder
        .decode(
            case.payload,
            Some(case.pts_microseconds),
            &mut work,
            &cancel,
        )
        .unwrap()
        .subtitle()
        .expect("presented display set");
    assert_eq!(
        (subtitle.start_display_ms, subtitle.end_display_ms),
        (0, 2000)
    );
    let rects: Vec<_> = subtitle.rects().collect();
    assert_eq!(rects.len(), 1);
    let rect = &rects[0];
    let geometry = [
        rect.x,
        rect.y,
        i32::from(rect.width),
        i32::from(rect.height),
    ];
    assert_eq!(geometry.map(|v| v as u32), case.rect);
    assert_eq!(rect.pixels, case.indices);
    assert_eq!(rect.palette, case.receiver_words, "{}", case.id);
    let rgba: Vec<u8> = rect
        .palette
        .iter()
        .flat_map(|w| [(w >> 16) as u8, (w >> 8) as u8, *w as u8, (w >> 24) as u8])
        .collect();
    assert_eq!(rgba, case.native_rgba, "{}", case.id);
}

#[test]
fn absent_cds_preserves_actual_automatic_palette_and_unused_entries() {
    check(0);
}

#[test]
fn partial_four_bit_palette_preserves_fixed_unredefined_entries() {
    check(1);
}

#[test]
fn partial_eight_bit_palette_preserves_all256_entries() {
    check(2);
}
