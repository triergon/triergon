//! A filled region without object data: the receiver presents an empty
//! subtitle wherever the region lies, the source decoder refuses a region
//! outside the display window.
#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};

/// Page with region 1 at (0, 0), a filled 2x2 region 1 and an end of display set.
const EMPTY: &[u8] = &[
    15, 16, 0, 1, 0, 8, 2, 11, 1, 255, 0, 0, 0, 0, 15, 17, 0, 1, 0, 10, 1, 15, 0, 2, 0, 2, 39, 0,
    0, 3, 15, 128, 0, 1, 0, 0,
];

fn payload(x: u16, y: u16) -> Vec<u8> {
    let mut bytes = EMPTY.to_vec();
    bytes[10..12].copy_from_slice(&x.to_be_bytes());
    bytes[12..14].copy_from_slice(&y.to_be_bytes());
    bytes
}

#[test]
fn receiver_presents_an_empty_subtitle_for_an_undrawn_region() {
    for (x, y) in [(719, 0), (0, 575), (1, 2)] {
        let pool = Budget::new(4 << 20).unwrap();
        let mut decoder = ReceiverDecoder::new(&pool, &[0, 1, 0, 2, 0x14], Options::default());
        let mut work = WorkBudget::new(u64::MAX);
        let subtitle = decoder
            .decode(
                &payload(x, y),
                Some(1_000_000),
                &mut work,
                &AtomicBool::new(false),
            )
            .unwrap()
            .subtitle()
            .expect("presented display set");
        assert_eq!(subtitle.pts, Some(1_000_000));
        assert_eq!(
            (subtitle.start_display_ms, subtitle.end_display_ms),
            (0, 2000)
        );
        assert_eq!(subtitle.rects().len(), 0);
        assert!(subtitle.display_size.is_none());
    }
}

#[test]
fn receiver_presents_nothing_for_a_segment_past_the_packet() {
    let pool = Budget::new(4 << 20).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
    let mut source = payload(719, 0);
    source[18..20].copy_from_slice(&u16::MAX.to_be_bytes());
    let mut work = WorkBudget::new(u64::MAX);
    let decoded = decoder.decode(&source, None, &mut work, &AtomicBool::new(false));
    assert!(decoded.unwrap().subtitle().is_none());
}
