//! Display state across display sets: the receiver decoder against recorded
//! output of the reference decoder.
#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};

struct SourcePacket {
    body: &'static [u8],
    /// Milliseconds.
    pts: i64,
}

struct Rect {
    geometry: [u32; 4],
    indices: &'static [u8],
    receiver_words: &'static [u32],
}

struct Expected {
    pts_us: i64,
    start: u32,
    end: u32,
    canvas: [u32; 2],
    rects: &'static [Rect],
}

struct Case {
    id: &'static str,
    packets: &'static [SourcePacket],
    events: &'static [Expected],
}

#[path = "functional-state-fixtures/cases.rs"]
mod cases;

fn run(index: usize) {
    let case = &cases::ALL[index];
    let pool = Budget::new(4 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &[0, 1, 0, 2, 0x14], Options::default());
    let mut expected = case.events.iter();
    for packet in case.packets {
        let mut work = WorkBudget::new(u64::MAX);
        let Some(subtitle) = decoder
            .decode(packet.body, Some(packet.pts * 1000), &mut work, &cancel)
            .unwrap()
        else {
            continue;
        };
        let want = expected
            .next()
            .unwrap_or_else(|| panic!("{}: extra subtitle", case.id));
        assert_eq!(subtitle.pts, Some(want.pts_us), "{}", case.id);
        assert_eq!(
            [subtitle.start_display_ms, subtitle.end_display_ms],
            [want.start, want.end]
        );
        if let Some(size) = subtitle.display_size {
            assert_eq!([size.width, size.height], want.canvas, "{}", case.id);
        }
        let rects: Vec<_> = subtitle.rects().collect();
        assert_eq!(rects.len(), want.rects.len(), "{}", case.id);
        for (rect, want) in rects.iter().zip(want.rects) {
            let geometry = [
                rect.x,
                rect.y,
                i32::from(rect.width),
                i32::from(rect.height),
            ];
            assert_eq!(geometry.map(|v| v as u32), want.geometry, "{}", case.id);
            assert_eq!(rect.pixels, want.indices, "{}", case.id);
            assert_eq!(rect.palette, want.receiver_words, "{}", case.id);
        }
    }
    assert!(expected.next().is_none(), "{}: missing subtitle", case.id);
}

include!("functional-state-fixtures/names.rs");
