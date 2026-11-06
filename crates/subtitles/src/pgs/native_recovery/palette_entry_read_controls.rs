//! Literal palette facts, boundary reads, funding and retained-owner controls.
use super::*;
use triergon_core::TimeBase;

fn segment(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
}

fn pcs(height: u16, picture: bool, state: u8) -> Vec<u8> {
    let [hi, lo] = height.to_be_bytes();
    let mut body = vec![0, 16, hi, lo, 0x10, 0, 1, state, 0, 0, u8::from(picture)];
    if picture {
        body.extend_from_slice(&[0, 7, 0, 0, 0, 0, 0, 0]);
    }
    let mut out = Vec::new();
    segment(&mut out, 0x16, &body);
    out
}

fn full_entries(reverse: bool) -> Vec<u8> {
    let mut entries = Vec::new();
    for index in 0..=255u8 {
        let white = (index & 1 != 0) ^ reverse;
        let alpha = if reverse { 255 - index } else { index };
        entries.extend_from_slice(&[index, if white { 235 } else { 16 }, 128, 128, alpha]);
    }
    entries
}

fn packet(height: u16, picture: bool, state: u8, entries: &[u8]) -> Vec<u8> {
    let mut out = pcs(height, picture, state);
    let mut palette = vec![0, 9];
    palette.extend_from_slice(entries);
    segment(&mut out, 0x14, &palette);
    if picture {
        segment(
            &mut out,
            0x15,
            &[0, 7, 0, 0xc0, 0, 0, 7, 0, 1, 0, 1, 1, 0, 0],
        );
    }
    segment(&mut out, 0x80, &[]);
    out
}

fn clock() -> PacketClock {
    PacketClock {
        pts: Some(90_000),
        dts: Some(-17),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn push(d: &mut IndexedNativeRecoveryDecoder, b: &Budget, data: &[u8]) -> Bytes {
    let held = Bytes::copy_from_slice(b, data).unwrap();
    let mut input = Some(held.clone());
    d.push(&mut input, clock(), &AtomicBool::new(false))
        .unwrap();
    assert!(input.is_none());
    held
}

fn decoder(b: &Budget) -> IndexedNativeRecoveryDecoder {
    IndexedNativeRecoveryDecoder::new(b, DisplayLimits::default(), DisplayConfig::default())
        .unwrap()
}

fn finish(d: &mut IndexedNativeRecoveryDecoder, quantum: u64) -> IndexedRecoveredPacket {
    for _ in 0..200_000 {
        if d.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
            == RecoveryStatus::OutputReady
        {
            return d.take_output(&AtomicBool::new(false)).unwrap();
        }
    }
    panic!("bounded palette completion");
}

fn reach_palette(d: &mut IndexedNativeRecoveryDecoder) {
    for _ in 0..200_000 {
        if d.phase == Phase::Palette && d.palette_at < d.palette_end {
            return;
        }
        assert_ne!(
            d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap(),
            RecoveryStatus::OutputReady
        );
    }
    panic!("bounded first palette entry");
}

fn literal_word(index: u8, reverse: bool) -> u32 {
    let white = (index & 1 != 0) ^ reverse;
    let alpha = if reverse { 255 - index } else { index };
    (u32::from(alpha) << 24) | if white { 0x00ff_ffff } else { 0 }
}

#[test]
fn complete_256_entry_palette_keeps_every_literal_source_and_receiver_fact() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b);
    let data = packet(32, true, 0x80, &full_entries(false));
    let held = push(&mut d, &b, &data);
    let out = finish(&mut d, 7);
    assert_eq!(out.source_error(), None);
    assert_eq!(out.clock, clock());
    assert_eq!(out.raw_packet().as_ref(), data.as_slice());
    assert!(out.raw_packet().shares_storage_with(&held));
    assert_eq!(out.source_sets().len(), 1);
    assert_eq!(out.receiver_sets().len(), 1);
    assert_eq!(out.receiver_sets()[0].pts_microseconds, Some(1_000_000));
    let source = out.source_sets()[0].objects()[0].palette();
    let receiver = out.receiver_sets()[0].rectangles()[0].palette();
    for index in 0..=255u8 {
        let colour = source.source(index).unwrap();
        assert_eq!(
            (colour.y, colour.cr, colour.cb, colour.alpha),
            (if index & 1 != 0 { 235 } else { 16 }, 128, 128, index)
        );
        assert_eq!(colour.palette_version, 9);
        assert_eq!(colour.epoch, 1);
        assert_eq!(colour.receiver_matrix, NativeMatrix::Sd601);
        assert_eq!(colour.stamp.packet_clock, clock());
        assert_eq!(source.native_argb(index), literal_word(index, false));
        assert_eq!(receiver[usize::from(index)], literal_word(index, false));
    }
    assert_eq!(out.receiver_observations.palette_cross_segment_bytes, 0);
    assert_eq!(out.receiver_observations.palette_zero_padding_bytes, 0);
}

#[test]
fn partial_entry_tails_keep_literal_cross_segment_and_api_padding_facts() {
    // Rows: packet end, END, unknown segment, and an empty following PDS.
    // Each column is the literal ARGB word for a one/two/three/four-byte tail.
    let words = [
        [0x0000_8800, 0x0033_ff00, 0x00ff_ff00, 0x00ff_ffff],
        [0x0000_ff00, 0x00ff_ff00, 0x00ff_ffff, 0x80ff_ffff],
        [0x0900_d400, 0x029c_ff00, 0x00ff_ff82, 0x42ff_ffff],
        [0x0000_9e00, 0x0253_ff00, 0x00ff_ff25, 0x14ff_ffff],
    ];
    let follows: [&[u8]; 4] = [
        &[],
        &[0x80, 0, 0],
        &[0x42, 0, 2, 9, 13, 0x80, 0, 0],
        &[0x14, 0, 2, 0, 0, 0x80, 0, 0],
    ];
    for (row, follow) in follows.iter().enumerate() {
        for tail_len in 1..=4 {
            let b = Budget::new(4 * 1024 * 1024).unwrap();
            let mut d = decoder(&b);
            let mut data = pcs(32, false, 0x80);
            let mut body = vec![0, 9, 42, 16, 128, 128, 255];
            body.extend_from_slice(&[211, 235, 128, 128][..tail_len]);
            segment(&mut data, 0x14, &body);
            data.extend_from_slice(follow);
            let held = push(&mut d, &b, &data);
            let out = finish(&mut d, 1);
            assert!(out.source_error().is_some());
            assert_eq!(d.palettes.as_slice()[0].words[42], 0xff00_0000);
            assert_eq!(
                d.palettes.as_slice()[0].words[211],
                words[row][tail_len - 1]
            );
            assert_eq!(
                out.receiver_observations.palette_cross_segment_bytes,
                5 - tail_len
            );
            assert_eq!(
                out.receiver_observations.palette_zero_padding_bytes,
                (5 - tail_len).saturating_sub(follow.len())
            );
            assert!(out.raw_packet().shares_storage_with(&held));
            assert_eq!(out.raw_packet().as_ref(), data.as_slice());
        }
    }
}

#[test]
fn duplicate_entries_keep_wire_order_and_each_canvas_matrix() {
    for (height, matrix, expected) in [
        (32, NativeMatrix::Sd601, 0xfffe_0000),
        (577, NativeMatrix::Hd709, 0xffff_1800),
    ] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let mut d = decoder(&b);
        let mut entries = full_entries(false);
        entries.extend_from_slice(&[7, 16, 128, 128, 13, 7, 81, 240, 90, 255]);
        let _held = push(&mut d, &b, &packet(height, true, 0x80, &entries));
        let out = finish(&mut d, 8);
        assert_eq!(out.source_error(), None);
        let source = out.source_sets()[0].objects()[0].palette();
        let colour = source.source(7).unwrap();
        assert_eq!(
            (colour.y, colour.cr, colour.cb, colour.alpha),
            (81, 240, 90, 255)
        );
        assert_eq!(colour.receiver_matrix, matrix);
        assert_eq!(source.native_argb(7), expected);
        assert_eq!(
            out.receiver_sets()[0].rectangles()[0].palette()[7],
            expected
        );
        assert_eq!(
            out.receiver_sets()[0].rectangles()[0].palette()[1],
            0x01ff_ffff
        );
    }
}

#[test]
fn entry_funding_work0_and_full_pool_pressure_do_not_publish_early() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b);
    let data = packet(32, false, 0x80, &[42, 16, 128, 128, 255]);
    let held = push(&mut d, &b, &data);
    reach_palette(&mut d);
    let (at, spent, previous_credit) = (d.palette_at, d.spent, d.credit);
    let unpaid = 8 - previous_credit;
    assert!(unpaid > 0);
    let pressure = b.reserve(b.available()).unwrap();
    let used = b.used();
    assert_eq!(
        d.poll(
            &mut WorkBudget::new((unpaid - 1) as u64),
            &AtomicBool::new(false)
        )
        .unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!(
        (d.palette_at, d.spent, d.credit),
        (at, spent + unpaid - 1, 7)
    );
    assert_eq!(d.palettes.as_slice()[0].words[42], 0);
    for _ in 0..3 {
        assert_eq!(
            d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            RecoveryStatus::Yield
        );
        assert_eq!(
            (d.palette_at, d.spent, d.credit, b.used()),
            (at, spent + unpaid - 1, 7, used)
        );
        assert!(d.packet.as_ref().unwrap().shares_storage_with(&held));
    }
    assert_eq!(
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!(
        (d.palette_at, d.spent, d.credit),
        (at + 5, spent + unpaid, 0)
    );
    assert_eq!(d.palettes.as_slice()[0].words[42], 0xff00_0000);
    assert_eq!(b.used(), used);
    drop(pressure);
    let out = finish(&mut d, 7);
    assert_eq!(out.source_error(), None);
    assert!(out.raw_packet().shares_storage_with(&held));
}

#[test]
fn cancellation_of_partially_funded_entry_precedes_work0_and_releases_storage() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = decoder(&b);
    let data = packet(32, false, 0x80, &[42, 16, 128, 128, 255]);
    let held = push(&mut d, &b, &data);
    reach_palette(&mut d);
    let unpaid = 8 - d.credit;
    assert!(unpaid > 0);
    d.poll(
        &mut WorkBudget::new((unpaid - 1) as u64),
        &AtomicBool::new(false),
    )
    .unwrap();
    let spent = d.spent;
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(d.spent, spent);
    assert!(d.packet.is_none() && d.palettes.is_empty());
    assert_eq!(held.as_ref(), data.as_slice());
    assert_eq!(
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    );
    drop(d);
    assert_eq!(b.used(), baseline + held.retained_storage_bytes());
    drop(held);
    assert_eq!(b.used(), baseline);
}

#[test]
fn dense_clear_then_image_keeps_the_unfolded_receiver_event_and_source_set() {
    let b = Budget::new(8 * 1024 * 1024).unwrap();
    let mut d = decoder(&b);
    let mut data = packet(32, false, 0x80, &full_entries(false));
    data.extend_from_slice(&packet(64, true, 0, &full_entries(true)));
    let held = push(&mut d, &b, &data);
    let out = finish(&mut d, 7);
    assert_eq!(out.source_error(), None);
    assert_eq!(out.source_sets().len(), 2);
    assert!(out.source_sets()[0].is_clear());
    assert_eq!(out.source_sets()[1].objects().len(), 1);
    assert_eq!(out.receiver_sets().len(), 2);
    assert!(out.receiver_output().unwrap().rectangles().is_empty());
    assert_eq!(out.omitted_receiver_events(), 1);
    assert_eq!(
        out.receiver_sets()[1].rectangles()[0].pixels().as_ref(),
        &[1]
    );
    assert_eq!(out.context_canvas, [16, 64]);
    assert!(out.raw_packet().shares_storage_with(&held));
    assert_eq!(out.raw_packet().as_ref(), data.as_slice());
    d.reset(&AtomicBool::new(false)).unwrap();
    drop(d);
    drop(held);
    for index in 0..=255u8 {
        assert_eq!(
            out.source_sets()[1].objects()[0]
                .palette()
                .native_argb(index),
            literal_word(index, true)
        );
        assert_eq!(
            out.receiver_sets()[1].rectangles()[0].palette()[usize::from(index)],
            literal_word(index, true)
        );
    }
}

#[test]
fn full_palette_work_and_retained_snapshots_survive_later_updates_reset_and_drop() {
    let mut expected_work = None;
    for quantum in [1, 7, 8, 255, 4096] {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let baseline = b.used();
        let mut d = decoder(&b);
        let raw1 = push(&mut d, &b, &packet(32, true, 0x80, &full_entries(false)));
        let first = finish(&mut d, quantum);
        let first_work = d.work_spent();
        let raw2 = push(&mut d, &b, &packet(32, true, 0, &full_entries(true)));
        let second = finish(&mut d, quantum);
        assert_eq!(first.source_error(), None);
        assert_eq!(second.source_error(), None);
        match expected_work {
            None => expected_work = Some((first_work, d.work_spent())),
            Some(pair) => assert_eq!((first_work, d.work_spent()), pair),
        }
        d.reset(&AtomicBool::new(false)).unwrap();
        drop(d);
        drop(raw1);
        drop(raw2);
        for index in 0..=255u8 {
            assert_eq!(
                first.receiver_sets()[0].rectangles()[0].palette()[usize::from(index)],
                literal_word(index, false)
            );
            assert_eq!(
                second.receiver_sets()[0].rectangles()[0].palette()[usize::from(index)],
                literal_word(index, true)
            );
            assert_eq!(
                first.source_sets()[0].objects()[0]
                    .palette()
                    .native_argb(index),
                literal_word(index, false)
            );
            assert_eq!(
                second.source_sets()[0].objects()[0]
                    .palette()
                    .native_argb(index),
                literal_word(index, true)
            );
        }
        assert!(b.used() > baseline);
        drop(first);
        drop(second);
        assert_eq!(b.used(), baseline);
    }
}
