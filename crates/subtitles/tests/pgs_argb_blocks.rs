//! Independent public materialized/indexed equivalence at every channel and
//! four-pixel block boundary. No timing threshold or output waiver.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayDecoder, DisplayLimits, DisplayStatus, Envelope, IndexedDisplayDecoder,
    NativeArgbView, PacketClock,
};

fn segment(raw: &mut Vec<u8>, kind: u8, body: &[u8]) {
    raw.push(kind);
    raw.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    raw.extend_from_slice(body);
}

fn packet() -> Vec<u8> {
    let mut raw = Vec::new();
    segment(
        &mut raw,
        0x16,
        &[
            1, 0, 0, 1, 0x10, 0, 1, 0x80, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0,
        ],
    );
    let mut palette = vec![0, 0];
    let mut encoded = Vec::new();
    for i in 0..=255u8 {
        palette.extend_from_slice(&[
            i,
            i.wrapping_mul(13),
            i.wrapping_mul(17),
            i.wrapping_mul(23),
            i ^ 0x95,
        ]);
        if i == 0 {
            encoded.extend_from_slice(&[0, 0x81, 0]);
        } else {
            encoded.push(i);
        }
    }
    encoded.extend_from_slice(&[0, 0]);
    segment(&mut raw, 0x14, &palette);
    let size = u32::try_from(encoded.len() + 4).unwrap().to_be_bytes();
    let mut object = vec![0, 7, 0, 0xc0, size[1], size[2], size[3], 1, 0, 0, 1];
    object.extend_from_slice(&encoded);
    segment(&mut raw, 0x15, &object);
    segment(&mut raw, 0x80, &[]);
    raw
}

fn views(b: &Budget, suppressed: bool) -> (NativeArgbView, Vec<u8>) {
    let config = DisplayConfig {
        native_forced_only: suppressed,
    };
    let clock = PacketClock {
        pts: None,
        dts: Some(-1),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    };
    let raw = Bytes::copy_from_slice(b, &packet()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut eager = DisplayDecoder::new(b, DisplayLimits::default(), config).unwrap();
    let mut indexed = IndexedDisplayDecoder::new(b, DisplayLimits::default(), config).unwrap();
    eager
        .push(raw.clone(), clock, Envelope::Packet, &cancel)
        .unwrap();
    indexed.push(raw, clock, Envelope::Packet, &cancel).unwrap();
    let mut eager_ready = false;
    let mut indexed_ready = false;
    for _ in 0..100_000 {
        if !eager_ready {
            eager_ready = eager.poll(&mut WorkBudget::new(257), &cancel).unwrap()
                == DisplayStatus::OutputReady;
        }
        if !indexed_ready {
            indexed_ready = indexed.poll(&mut WorkBudget::new(257), &cancel).unwrap()
                == DisplayStatus::OutputReady;
        }
        if eager_ready && indexed_ready {
            let expected = eager.take_output(&cancel).unwrap().objects()[0]
                .native_argb_pixels()
                .as_ref()
                .to_vec();
            let source = indexed.take_output(&cancel).unwrap();
            let view = source.objects()[0].native_argb_view().unwrap();
            assert_eq!(view.len(), 1024);
            assert!(view.palette().source(0).is_some());
            assert!(view.palette().source(255).is_some());
            return (view, expected);
        }
    }
    panic!("bounded full-palette fixture progress");
}

fn advance(writer: &mut triergon_subtitles::pgs::ArgbWriter, offset: usize) {
    let mut remaining = offset;
    let mut target = [0; 256];
    while remaining != 0 {
        let count = remaining.min(target.len());
        let step = writer
            .write(
                &mut target[..count],
                &mut WorkBudget::new(256),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(step.written, count);
        remaining -= count;
    }
}

#[test]
fn all_1024_start_offsets_match_materialized_palette_and_channel_order() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let (view, expected) = views(&b, false);
    let charged = b.used();
    for offset in 0..view.len() {
        let mut writer = view.clone().writer(view.len());
        advance(&mut writer, offset);
        let mut target = [0xa5; 258];
        let step = writer
            .write(
                &mut target[1..257],
                &mut WorkBudget::new(256),
                &AtomicBool::new(false),
            )
            .unwrap();
        let count = (view.len() - offset).min(256);
        assert_eq!(step.written, count);
        assert_eq!(&target[1..1 + count], &expected[offset..offset + count]);
        assert!(target[1 + count..].iter().all(|v| *v == 0xa5));
        assert_eq!(target[0], 0xa5);
        assert_eq!(writer.work_spent(), offset + count);
        assert_eq!(b.used(), charged);
    }
}

#[test]
fn every_target_length_and_channel_alignment_preserves_unfunded_suffix() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let (view, expected) = views(&b, false);
    for offset in 0..4 {
        for length in 0..=256 {
            let mut writer = view.clone().writer(view.len());
            advance(&mut writer, offset);
            let mut target = [0xa5; 258];
            let mut work = WorkBudget::new(u64::try_from(length).unwrap());
            let step = writer
                .write(
                    &mut target[1..1 + length],
                    &mut work,
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert_eq!(step.written, length);
            assert_eq!(&target[1..1 + length], &expected[offset..offset + length]);
            assert!(target[1 + length..].iter().all(|v| *v == 0xa5));
            assert_eq!(target[0], 0xa5);
            assert_eq!(work.spent(), u64::try_from(length).unwrap());
            assert_eq!(writer.offset(), offset + length);
        }
    }
}

#[test]
fn one_unit_writes_every_byte_across_each_sixteen_byte_boundary() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let (view, expected) = views(&b, false);
    let mut writer = view.writer(expected.len());
    for (offset, byte) in expected.iter().copied().enumerate() {
        let mut target = [0xa5; 17];
        let mut work = WorkBudget::new(1);
        let step = writer
            .write(&mut target, &mut work, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(step.written, 1);
        assert_eq!(target[0], byte);
        assert!(target[1..].iter().all(|v| *v == 0xa5));
        assert_eq!(work.spent(), 1);
        assert_eq!(writer.work_spent(), offset + 1);
        assert_eq!(step.complete, offset + 1 == expected.len());
    }
}

#[test]
fn zero_work_and_partial_block_limit_leave_unfunded_bytes_untouched() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let (view, expected) = views(&b, false);
    let mut writer = view.writer(19);
    let mut target = [0xa5; 32];
    let cancel = AtomicBool::new(false);
    assert_eq!(
        writer
            .write(&mut target, &mut WorkBudget::new(0), &cancel)
            .unwrap()
            .written,
        0
    );
    assert_eq!(target, [0xa5; 32]);
    assert_eq!(writer.offset(), 0);
    assert_eq!(
        writer
            .write(&mut target, &mut WorkBudget::new(100), &cancel)
            .unwrap()
            .written,
        19
    );
    assert_eq!(&target[..19], &expected[..19]);
    assert!(target[19..].iter().all(|v| *v == 0xa5));
    target.fill(0xa5);
    let error = Error::Limit {
        requested: 20,
        maximum: 19,
    };
    assert_eq!(
        writer.write(&mut target, &mut WorkBudget::new(100), &cancel),
        Err(error)
    );
    assert_eq!(
        writer.write(&mut target, &mut WorkBudget::new(0), &cancel),
        Err(error)
    );
    assert_eq!(target, [0xa5; 32]);
    assert_eq!(writer.offset(), 19);
}

#[test]
fn suppressed_bulk_output_retains_known_source_and_releases_owned_charge() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let (view, expected) = views(&b, true);
    assert!(expected.iter().all(|v| *v == 0));
    assert!(view.palette().source(255).is_some());
    let mut writer = view.writer(expected.len());
    for length in [1, 3, 15, 16, 17, 63, 64, 65, 255, 256, 256, 256] {
        let mut target = [0xa5; 258];
        let step = writer
            .write(
                &mut target[1..1 + length],
                &mut WorkBudget::new(256),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(target[1..1 + step.written].iter().all(|v| *v == 0));
        assert!(target[1 + step.written..].iter().all(|v| *v == 0xa5));
        assert_eq!(target[0], 0xa5);
        if step.complete {
            break;
        }
    }
    assert!(writer.is_complete());
    assert!(b.used() > baseline);
    drop(writer);
    assert_eq!(b.used(), baseline);
}
