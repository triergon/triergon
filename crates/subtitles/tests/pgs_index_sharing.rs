//! Public-API guards for pointer/budget sharing. No validation API or
//! expected-input lookup is used.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayLimits, IndexedNativeRecoveryDecoder, IndexedRecoveredPacket,
    NativeColourOrigin, PacketClock, RecoveryStatus,
};

fn segment(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
}

fn frame(width: u16, colour: u8, state: u8, palette: bool, tail: bool) -> Vec<u8> {
    let [hi, lo] = width.to_be_bytes();
    let mut out = Vec::new();
    segment(
        &mut out,
        0x16,
        &[
            hi, lo, 0, 1, 0x10, 0, 1, state, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0,
        ],
    );
    if palette {
        segment(
            &mut out,
            0x14,
            &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128],
        );
    } else {
        segment(&mut out, 0x14, &[0, 1]);
    }
    let mut rle = vec![0, 0xc0 | (hi & 0x3f), lo, colour, 0, 0];
    if tail {
        rle.push(1);
    }
    let size = u32::try_from(rle.len() + 4).unwrap().to_be_bytes();
    let mut object = vec![0, 7, 0, 0xc0, size[1], size[2], size[3], hi, lo, 0, 1];
    object.extend_from_slice(&rle);
    segment(&mut out, 0x15, &object);
    segment(&mut out, 0x80, &[]);
    out
}

fn clock() -> PacketClock {
    PacketClock {
        pts: None,
        dts: Some(-7),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn decoder(b: &Budget, forced: bool) -> IndexedNativeRecoveryDecoder {
    IndexedNativeRecoveryDecoder::new(
        b,
        DisplayLimits::default(),
        DisplayConfig {
            native_forced_only: forced,
        },
    )
    .unwrap()
}

fn read(
    d: &mut IndexedNativeRecoveryDecoder,
    b: &Budget,
    raw: &[u8],
    quantum: u64,
) -> IndexedRecoveredPacket {
    let cancel = AtomicBool::new(false);
    let mut input = Some(Bytes::copy_from_slice(b, raw).unwrap());
    d.push(&mut input, clock(), &cancel).unwrap();
    for _ in 0..1_000_000 {
        if d.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() == RecoveryStatus::OutputReady {
            return d.take_output(&cancel).unwrap();
        }
    }
    panic!("bounded index-sharing fixture progress");
}

#[test]
fn compressible_4096_source_and_receiver_retain_exactly_one_index_backing() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let raw = frame(4096, 1, 0x80, true, false);
    let out = read(&mut d, &b, &raw, 1);
    assert_eq!(out.source_error(), None);
    let source = out.source_sets()[0].objects()[0].pixels();
    let receiver = out.receiver_sets()[0].rectangles()[0].pixels();
    assert_eq!(source.len(), 4096);
    assert!(source.iter().all(|v| *v == 1));
    assert_eq!(
        source.as_ptr(),
        receiver.as_ptr(),
        "v3 allocates a second plane; v4 must share actual owner"
    );
    assert_eq!(out.raw_packet().as_ref(), raw);
    assert_eq!(out.clock, clock());
}

#[test]
fn compressible_16383_all_indices_share_without_changing_source_or_receiver_values() {
    for colour in [0, 1, 255] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let mut d = decoder(&b, false);
        let out = read(&mut d, &b, &frame(16383, colour, 0x80, true, false), 4096);
        let source = out.source_sets()[0].objects()[0].pixels();
        let receiver = out.receiver_sets()[0].rectangles()[0].pixels();
        assert_eq!(source.as_ref(), receiver.as_ref());
        assert_eq!(source.as_ptr(), receiver.as_ptr());
        assert!(receiver.iter().all(|v| *v == colour));
    }
}

#[test]
fn mismatching_same_id_same_dimensions_never_share_another_generation() {
    let mut raw = frame(4096, 1, 0x80, true, false);
    raw.extend_from_slice(&frame(4096, 2, 0, true, false));
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let out = read(&mut d, &b, &raw, 1);
    assert_eq!(out.source_sets().len(), 2);
    assert_eq!(out.receiver_sets().len(), 2);
    let a = out.receiver_sets()[0].rectangles()[0].pixels();
    let z = out.receiver_sets()[1].rectangles()[0].pixels();
    assert!(a.iter().all(|v| *v == 1));
    assert!(z.iter().all(|v| *v == 2));
    assert_ne!(a.as_ptr(), z.as_ptr());
    assert_ne!(
        a.as_ptr(),
        out.source_sets()[1].objects()[0].pixels().as_ptr()
    );
    assert_eq!(
        z.as_ptr(),
        out.source_sets()[1].objects()[0].pixels().as_ptr()
    );
}

#[test]
fn receiver_tail_acceptance_is_not_a_strict_rle_validation_proof() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let out = read(&mut d, &b, &frame(4096, 1, 0x80, true, true), 1);
    assert!(matches!(out.source_error(), Some(Error::Invalid(_))));
    assert!(out.source_sets().is_empty());
    let receiver = out.receiver_sets()[0].rectangles()[0].pixels();
    assert_eq!(receiver.len(), 4096);
    assert!(receiver.iter().all(|v| *v == 1));
}

#[test]
fn no_compact_encoding_evidence_uses_original_independent_receiver_path() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    // Encoding is6 bytes for4pixels: retaining it cannot reduce storage.
    let out = read(&mut d, &b, &frame(4, 1, 0x80, true, false), 1);
    let source = out.source_sets()[0].objects()[0].pixels();
    let receiver = out.receiver_sets()[0].rectangles()[0].pixels();
    assert_eq!(source.as_ref(), receiver.as_ref());
    assert_ne!(source.as_ptr(), receiver.as_ptr());
}

#[test]
fn small_rle_slice_never_retains_an_oversized_physical_packet_backing() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let mut raw = Vec::with_capacity(16384);
    raw.extend_from_slice(&frame(4096, 1, 0x80, true, false));
    let mut input = Some(Bytes::try_from_vec(&b, raw).unwrap());
    let cancel = AtomicBool::new(false);
    d.push(&mut input, clock(), &cancel).unwrap();
    for _ in 0..100_000 {
        if d.poll(&mut WorkBudget::new(1), &cancel).unwrap() == RecoveryStatus::OutputReady {
            let out = d.take_output(&cancel).unwrap();
            let source = out.source_sets()[0].objects()[0].pixels();
            let receiver = out.receiver_sets()[0].rectangles()[0].pixels();
            assert_eq!(source.as_ref(), receiver.as_ref());
            assert!(!source.shares_storage_with(receiver));
            assert!(out.raw_packet().retained_capacity() >= 16384);
            return;
        }
    }
    panic!("bounded oversized-backing fixture progress");
}

#[test]
fn epoch_and_palette_changes_never_promote_source_missingness() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let old = read(&mut d, &b, &frame(4096, 1, 0x80, true, false), 1);
    let new = read(&mut d, &b, &frame(4096, 1, 0x80, false, false), 1);
    let object = &new.source_sets()[0].objects()[0];
    assert_eq!(object.palette().source(1), None);
    assert_eq!(
        object.palette().native_origin(1),
        NativeColourOrigin::PreviousEpochSlot
    );
    assert_eq!(
        object.pixels().as_ptr(),
        new.receiver_sets()[0].rectangles()[0].pixels().as_ptr()
    );
    assert!(
        old.source_sets()[0].objects()[0]
            .palette()
            .source(1)
            .is_some()
    );
    assert_ne!(
        old.source_sets()[0].objects()[0].pixels().as_ptr(),
        object.pixels().as_ptr()
    );
}

#[test]
fn retained_receiver_pixels_survive_cancel_reset_and_source_cache_release() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = decoder(&b, false);
    let out = read(&mut d, &b, &frame(4096, 1, 0x80, true, false), 1);
    let pixels = out.receiver_sets()[0].rectangles()[0].pixels().clone();
    drop(out);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    d.reset(&cancel).unwrap();
    let fresh = read(&mut d, &b, &frame(4096, 2, 0x80, true, false), 1);
    assert!(pixels.iter().all(|v| *v == 1));
    assert!(
        fresh.receiver_sets()[0].rectangles()[0]
            .pixels()
            .iter()
            .all(|v| *v == 2)
    );
    drop(fresh);
    drop(d);
    assert!(b.used() > baseline);
    drop(pixels);
    assert_eq!(b.used(), baseline);
}

#[test]
fn every_original_argb_byte_is_still_written_with_one_unit_quanta() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, true);
    let out = read(&mut d, &b, &frame(4096, 1, 0x80, true, false), 1);
    let object = &out.source_sets()[0].objects()[0];
    assert!(object.palette().source(1).is_some());
    let mut writer = object.native_argb_view().unwrap().writer(16384);
    for _ in 0..16384 {
        let mut target = [0xaa; 4];
        let before = d.work_spent();
        let step = d
            .write_source_argb(
                &mut writer,
                &mut target,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(step.written, 1);
        assert_eq!(target, [0, 0xaa, 0xaa, 0xaa]);
        assert_eq!(d.work_spent(), before + 1);
    }
    assert_eq!(writer.offset(), 16384);
}
