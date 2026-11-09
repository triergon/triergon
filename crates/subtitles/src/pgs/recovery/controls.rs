//! Engine guards that need the private palette representation or that drive
//! allocation pressure into every allocation of one packet.
use super::*;
use crate::pgs::receiver_decoder::NativeOnly;
use triergon_core::TimeBase;

fn segment(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
}

/// A 16x1 PCS with `state` referencing object 7 once per `(flags, x)`.
fn composition(out: &mut Vec<u8>, state: u8, references: &[(u8, u16)]) {
    let mut pcs = vec![0, 16, 0, 1, 0x10, 0, 1, state, 0, 0];
    pcs.push(u8::try_from(references.len()).unwrap());
    for &(flags, x) in references {
        pcs.extend_from_slice(&[0, 7, 0, flags]);
        pcs.extend_from_slice(&x.to_be_bytes());
        pcs.extend_from_slice(&[0, 0]);
    }
    segment(out, 0x16, &pcs);
}

/// A 16x1 epoch referencing object 7 once per `(flags, x)`: palette entries
/// 1 and 2 and a two-pixel object.
fn picture(references: &[(u8, u16)]) -> Vec<u8> {
    let mut out = Vec::new();
    composition(&mut out, 0x80, references);
    segment(
        &mut out,
        0x14,
        &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128],
    );
    segment(
        &mut out,
        0x15,
        &[0, 7, 0, 0xc0, 0, 0, 8, 0, 2, 0, 1, 1, 2, 0, 0],
    );
    segment(&mut out, 0x80, &[]);
    out
}

fn clock() -> PacketClock {
    PacketClock {
        pts: Some(90_000),
        dts: None,
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn decode<S: RecoveryRole>(d: &mut Decoder<S>, b: &Budget, raw: &[u8]) -> S::Packet {
    let cancel = AtomicBool::new(false);
    let mut input = Some(Bytes::copy_from_slice(b, raw).unwrap());
    d.push(&mut input, clock(), &cancel).unwrap();
    loop {
        if d.poll(&mut WorkBudget::new(4096), &cancel).unwrap() == RecoveryStatus::OutputReady {
            return d.take_output(&cancel).unwrap();
        }
    }
}

fn receiver(b: &Budget, forced_only: bool) -> Decoder<NativeOnly> {
    let config = DisplayConfig {
        receiver_forced_only: forced_only,
    };
    Decoder::new(b, DisplayLimits::default(), config).unwrap()
}

#[test]
fn a_display_shares_the_strict_palette_when_its_words_are_equal() {
    let b = Budget::new(1 << 20).unwrap();
    let mut d =
        RecoveryDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let out = decode(&mut d, &b, &picture(&[(0, 3)]));
    let rect = &out.receiver_output().unwrap().rectangles()[0];
    assert!(matches!(rect.palette, ReceiverPalette::Shared(_)));
    assert_eq!(
        (rect.palette()[1], rect.palette()[2]),
        (0xffff_ffff, 0x8000_0000)
    );
}

#[test]
fn rectangles_share_one_owned_palette_and_forced_only_hides_the_rest() {
    let b = Budget::new(1 << 20).unwrap();
    let mut d = receiver(&b, true);
    let out = decode(&mut d, &b, &picture(&[(0x40, 3), (0, 5)]));
    let rects = out.receiver_output().unwrap().rectangles();
    assert_eq!(rects.len(), 2);
    let ReceiverPalette::Owned(owned) = &rects[0].palette else {
        panic!("forced rectangle without the composition palette");
    };
    assert_eq!(owned.words[1], 0xffff_ffff);
    assert!(matches!(rects[1].palette, ReceiverPalette::Hidden));
    assert_eq!(rects[1].palette(), &[0; 256]);
    assert_eq!((rects[1].x, rects[1].pixels().as_ref()), (5, &[1, 2][..]));
}

#[test]
fn pressure_at_every_allocation_retries_without_repeating_work() {
    let raw = picture(&[(0, 3)]);
    let expected = {
        let b = Budget::new(1 << 20).unwrap();
        let mut d = receiver(&b, false);
        let out = decode(&mut d, &b, &raw);
        let rect = &out.receiver_output().unwrap().rectangles()[0];
        (d.work_spent(), rect.pixels().to_vec(), *rect.palette())
    };
    let b = Budget::new(1 << 20).unwrap();
    let mut d = receiver(&b, false);
    let cancel = AtomicBool::new(false);
    let mut input = Some(Bytes::copy_from_slice(&b, &raw).unwrap());
    d.push(&mut input, clock(), &cancel).unwrap();
    let mut pressured = 0;
    let out = loop {
        let hold = b.reserve(b.available()).unwrap();
        match d.poll(&mut WorkBudget::new(4096), &cancel) {
            Err(Error::Backpressure { .. }) => pressured += 1,
            Ok(RecoveryStatus::OutputReady) => break d.take_output(&cancel).unwrap(),
            other => panic!("unexpected {other:?}"),
        }
        drop(hold);
        if d.poll(&mut WorkBudget::new(1), &cancel).unwrap() == RecoveryStatus::OutputReady {
            break d.take_output(&cancel).unwrap();
        }
    };
    assert!(pressured >= 3, "every allocation met pressure once");
    let rect = &out.receiver_output().unwrap().rectangles()[0];
    assert_eq!(
        (d.work_spent(), rect.pixels().to_vec(), *rect.palette()),
        expected
    );
}

/// Each presented rectangle as `(x, y, width, height, stride, pixels)`.
fn presented(packet: &crate::pgs::receiver_decoder::ReceiverPacket) -> Vec<Rectangle> {
    packet
        .receiver_output()
        .unwrap()
        .rectangles()
        .iter()
        .map(|r| (r.x, r.y, r.width, r.height, r.stride, r.pixels().to_vec()))
        .collect()
}
type Rectangle = (u16, u16, u16, u16, u16, Vec<u8>);

/// A display of object 7 at x 3 in the current epoch.
fn redisplay() -> Vec<u8> {
    let mut out = Vec::new();
    composition(&mut out, 0, &[(0, 3)]);
    segment(&mut out, 0x80, &[]);
    out
}

#[test]
fn an_object_fragment_refused_under_pressure_leaves_the_receiver_as_before() {
    let b = Budget::new(1 << 20).unwrap();
    let mut refused = receiver(&b, false);
    let mut untouched = receiver(&b, false);
    for d in [&mut refused, &mut untouched] {
        decode(d, &b, &picture(&[(0, 3)]));
    }
    // A first fragment that would replace object 7 with a 4x1 bitmap.
    let replacement = [0, 7, 0, 0xc0, 0, 0, 8, 0, 4, 0, 1, 3, 3, 3, 3];
    let hold = b.reserve(b.available()).unwrap();
    let result = refused
        .receiver
        .object_fragment(&replacement, &refused.limits, &refused.budget);
    assert!(matches!(result, Err(Error::Backpressure { .. })));
    drop(hold);
    let expected = presented(&decode(&mut untouched, &b, &redisplay()));
    assert_eq!(expected, [(3, 0, 2, 1, 2, vec![1, 2])]);
    assert_eq!(presented(&decode(&mut refused, &b, &redisplay())), expected);
    // Without pressure the same fragment does replace the object.
    refused
        .receiver
        .object_fragment(&replacement, &refused.limits, &refused.budget)
        .unwrap();
    assert_eq!(
        presented(&decode(&mut refused, &b, &redisplay())),
        [(3, 0, 4, 1, 4, vec![3; 4])]
    );
}
