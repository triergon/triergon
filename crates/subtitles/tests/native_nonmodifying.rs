#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    native_indices: &'static [u8],
    native_palette: &'static [u8],
    receiver_words: &'static [u32],
    non_modifying: bool,
    depth: u8,
    pts_raw: i64,
}
#[path = "native-nonmodifying-support/cases.rs"]
mod cases;
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
        .subtitle()
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
