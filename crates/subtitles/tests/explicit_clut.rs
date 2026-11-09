#![forbid(unsafe_code)]
use std::io::{Cursor, Read};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder};

/// One recorded frame: its packet and the reference decoder's output.
struct Frame {
    pts: i64,
    payload: Vec<u8>,
    plane: Vec<u8>,
    native: Vec<u32>,
}

struct Case {
    frames: Vec<Frame>,
}

fn bytes<const N: usize>(input: &mut Cursor<&[u8]>) -> [u8; N] {
    let mut out = [0; N];
    input.read_exact(&mut out).unwrap();
    out
}

fn vector(input: &mut Cursor<&[u8]>, count: usize) -> Vec<u8> {
    let mut out = vec![0; count];
    input.read_exact(&mut out).unwrap();
    out
}

/// Reads a case file, skipping the source facts it also records: per frame the
/// sequence, DTS, duration, position, CLUT facts and source and studio RGBA.
fn case(data: &[u8]) -> Case {
    let mut input = Cursor::new(data);
    assert_eq!(&bytes::<8>(&mut input), b"SMCE0001");
    let bits = bytes::<1>(&mut input)[0];
    assert!(matches!(bits, 2 | 4 | 8), "authored depth");
    let count = usize::from(u16::from_be_bytes(bytes(&mut input)));
    let n = 1_usize << bits;
    let mut frames = Vec::new();
    for _ in 0..count {
        bytes::<8>(&mut input);
        let pts = i64::from_be_bytes(bytes(&mut input));
        vector(&mut input, 24);
        let length = usize::try_from(u32::from_be_bytes(bytes(&mut input))).unwrap();
        let payload = vector(&mut input, length);
        vector(&mut input, n * (5 + 4 + 4));
        let plane = vector(&mut input, n * 2);
        let native = (0..n)
            .map(|_| u32::from_be_bytes(bytes(&mut input)))
            .collect();
        frames.push(Frame {
            pts,
            payload,
            plane,
            native,
        });
    }
    assert_eq!(usize::try_from(input.position()).unwrap(), data.len());
    Case { frames }
}

/// The receiver decoder against the recorded output of the reference
/// decoder: its ARGB words, the index plane and the subtitle clock.
fn native_guard(data: &[u8]) {
    let case = case(data);
    let pool = Budget::new(4 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &[0, 1, 0, 2, 0x10], Options::default());
    for frame in &case.frames {
        let mut work = WorkBudget::new(u64::MAX);
        let subtitle = decoder
            .decode(&frame.payload, Some(frame.pts * 1000), &mut work, &cancel)
            .unwrap()
            .subtitle()
            .expect("presented display set");
        assert_eq!(subtitle.pts, Some(frame.pts * 1000));
        assert_eq!(
            (subtitle.start_display_ms, subtitle.end_display_ms),
            (0, 2000)
        );
        let rects: Vec<_> = subtitle.rects().collect();
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].palette, frame.native);
        assert_eq!(rects[0].pixels, frame.plane);
    }
}

#[test]
fn full_rgb_rounding_and_clipping() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-rgb.case"));
}
#[test]
fn all_full_range_transparency_values() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-alpha.case"));
}
#[test]
fn unused_encoded_rgb_is_preserved() {
    native_guard(include_bytes!("explicit-clut-fixtures/unused-full.case"));
}
#[test]
fn full_range_two_bit_palette() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-depth2.case"));
}
#[test]
fn full_range_four_bit_palette() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-depth4.case"));
}
#[test]
fn reduced_nonfinal_conversion() {
    native_guard(include_bytes!(
        "explicit-clut-fixtures/reduced-nonfinal.case"
    ));
}
#[test]
fn reduced_final_fresh_keeps_native_default() {
    native_guard(include_bytes!(
        "explicit-clut-fixtures/reduced-final-fresh.case"
    ));
}
#[test]
fn reduced_final_retains_prior_two_bit_entry() {
    native_guard(include_bytes!("explicit-clut-fixtures/reduced-tail2.case"));
}
#[test]
fn reduced_final_retains_prior_four_bit_entry() {
    native_guard(include_bytes!("explicit-clut-fixtures/reduced-tail4.case"));
}
#[test]
fn reduced_final_retains_prior_eight_bit_entry() {
    native_guard(include_bytes!("explicit-clut-fixtures/reduced-tail8.case"));
}
#[test]
fn native_yzero_companions_keep_raw_facts() {
    native_guard(include_bytes!(
        "explicit-clut-fixtures/transparent-companions.case"
    ));
}
