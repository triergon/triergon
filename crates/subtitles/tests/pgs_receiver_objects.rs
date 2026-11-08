//! Receiver objects whose headers claim more than the receiver stores.
//!
//! The expected rectangles are what the pinned FFmpeg `pgssub` decoder
//! (libavcodec at d32b387f) returned for the same packets, with the packet PTS
//! 90 kHz ticks of one second per packet.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, TimeBase, WorkBudget};
use triergon_subtitles::compat::pgs::ReceiverDecoder;
use triergon_subtitles::pgs::{DisplayConfig, DisplayLimits, PacketClock, RecoveryStatus};

/// `(x, y, width, height, stride, pixels)` of one rectangle.
type Rectangle = (u16, u16, u16, u16, u16, Vec<u8>);

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// Each packet's presented rectangles, at every work quantum.
fn presented(packets: &[&str], limits: DisplayLimits) -> Vec<Vec<Rectangle>> {
    let mut runs = [1, 7, 4096].map(|quantum| {
        let budget = Budget::new(64 << 20).unwrap();
        let cancel = AtomicBool::new(false);
        let mut decoder = ReceiverDecoder::new(&budget, limits, DisplayConfig::default()).unwrap();
        let mut out = Vec::new();
        for (index, packet) in packets.iter().enumerate() {
            let clock = PacketClock {
                pts: Some(90_000 * (index as i64 + 1)),
                dts: None,
                timebase: TimeBase::new(1, 90_000).unwrap(),
            };
            let mut raw = Some(Bytes::copy_from_slice(&budget, &hex(packet)).unwrap());
            decoder.push(&mut raw, clock, &cancel).unwrap();
            let packet = loop {
                match decoder
                    .poll(&mut WorkBudget::new(quantum), &cancel)
                    .unwrap()
                {
                    RecoveryStatus::OutputReady => break decoder.take_output(&cancel).unwrap(),
                    RecoveryStatus::Yield => {}
                    status => panic!("packet {index}: {status:?}"),
                }
            };
            let event = packet.receiver_output().unwrap();
            assert_eq!(event.pts_microseconds, Some(1_000_000 * (index as i64 + 1)));
            out.push(
                event
                    .rectangles()
                    .iter()
                    .map(|r| (r.x, r.y, r.width, r.height, r.stride, r.pixels().to_vec()))
                    .collect(),
            );
        }
        out
    });
    assert!(runs.iter().all(|run| *run == runs[0]));
    std::mem::take(&mut runs[0])
}

#[test]
fn a_declared_length_past_the_rle_limit_is_a_claim_not_an_allocation() {
    // A 16x4 epoch shows object 1 (2x1) at x 3; then first fragments
    // declaring 0xffffff bytes, with complete and cut RLE; then a
    // continuation that completes the cut one.
    let packets = [
        "1600130010000410000080000001000100000003000014000c000001eb8080ff021080808015000f000100c00000080002000101010000800000",
        "1600130010000410000100000001000100000003000015000f000100c0ffffff0002000102020000800000",
        "1600130010000410000200000001000100000003000015000c000100c0ffffff0002000102800000",
        "16001300100004100003000000010001000000030000800000",
        "1500070001004002000016001300100004100004000000010001000000030000800000",
    ];
    let limits = DisplayLimits::default();
    assert!(limits.max_encoded < 0xff_ffff);
    assert_eq!(
        presented(&packets, limits),
        [
            vec![(3, 0, 2, 1, 2, vec![1, 1])],
            vec![(3, 0, 2, 1, 2, vec![2, 2])],
            vec![(3, 0, 0, 0, 2, vec![])],
            vec![(3, 0, 0, 0, 2, vec![])],
            vec![(3, 0, 2, 1, 2, vec![2, 2])],
        ]
    );
}

#[test]
fn an_object_past_the_pixel_limit_ends_nothing_until_it_is_displayed() {
    // A 4096x4096 epoch shows object 1 (2x1); a 4096x4096 first fragment
    // of object 1 is replaced in the same packet before its display.
    let packets = [
        "1600131000100010000080000001000100000003000014000c000001eb8080ff021080808015000f000100c00000080002000101010000800000",
        "1600131000100010000100000001000100000003000015000f000100c0000008100010000101000015000f000100c00000080002000102020000800000",
        "16001310001000100002000000010001000000030000800000",
    ];
    let limits = DisplayLimits::default();
    assert!(limits.max_object_pixels < 4096 * 4096);
    assert_eq!(
        presented(&packets, limits),
        [
            vec![(3, 0, 2, 1, 2, vec![1, 1])],
            vec![(3, 0, 2, 1, 2, vec![2, 2])],
            vec![(3, 0, 2, 1, 2, vec![2, 2])],
        ]
    );
}
