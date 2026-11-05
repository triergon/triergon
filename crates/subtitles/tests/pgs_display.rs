//! Original multiframe display/cache/retention guards. Literal source facts and
//! the separately named native projection are asserted independently.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Time, TimeBase, WorkBudget};
use triergon_subtitles::pgs::{
    CompositionState, DisplayConfig, DisplayDecoder, DisplayLimits, DisplaySet, DisplayStatus,
    Envelope, NativeClockSource, NativeColourOrigin, NativeMatrix, PacketClock,
};

fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![kind];
    v.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    v.extend_from_slice(body);
    v
}
type Reference = (u16, u8, u8, u16, u16, Option<(u16, u16, u16, u16)>);
fn pcs(state: u8, number: u16, palette: u8, refs: &[Reference]) -> Vec<u8> {
    let mut b = vec![0, 64, 0, 32, 0x10];
    b.extend_from_slice(&number.to_be_bytes());
    b.extend_from_slice(&[state, 0, palette, refs.len() as u8]);
    for &(id, window, flags, x, y, crop) in refs {
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&[window, flags]);
        b.extend_from_slice(&x.to_be_bytes());
        b.extend_from_slice(&y.to_be_bytes());
        if let Some((x, y, w, h)) = crop {
            for n in [x, y, w, h] {
                b.extend_from_slice(&n.to_be_bytes());
            }
        }
    }
    segment(0x16, &b)
}
fn wds(windows: &[(u8, u16, u16, u16, u16)]) -> Vec<u8> {
    let mut b = vec![windows.len() as u8];
    for &(id, x, y, w, h) in windows {
        b.push(id);
        for n in [x, y, w, h] {
            b.extend_from_slice(&n.to_be_bytes());
        }
    }
    segment(0x17, &b)
}
fn pds(id: u8, version: u8, colours: &[[u8; 5]]) -> Vec<u8> {
    let mut b = vec![id, version];
    for entry in colours {
        b.extend_from_slice(entry);
    }
    segment(0x14, &b)
}
fn ods(
    id: u16,
    version: u8,
    flags: u8,
    encoded: usize,
    width: u16,
    height: u16,
    data: &[u8],
) -> Vec<u8> {
    let mut b = id.to_be_bytes().to_vec();
    b.extend_from_slice(&[version, flags]);
    if flags & 0x80 != 0 {
        b.extend_from_slice(&(u32::try_from(encoded).unwrap() + 4).to_be_bytes()[1..]);
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
    }
    b.extend_from_slice(data);
    segment(0x15, &b)
}
fn rle(pixels: &[u8], width: usize) -> Vec<u8> {
    let mut b = Vec::new();
    for row in pixels.chunks(width) {
        for &x in row {
            if x == 0 {
                b.extend_from_slice(&[0, 1]);
            } else {
                b.push(x);
            }
        }
        b.extend_from_slice(&[0, 0]);
    }
    b
}
fn palette() -> Vec<u8> {
    pds(
        0,
        0,
        &[
            [1, 16, 128, 128, 255],
            [2, 235, 128, 128, 255],
            [3, 235, 128, 128, 0],
            [4, 81, 240, 90, 127],
            [5, 145, 34, 54, 255],
        ],
    )
}
fn frame(state: u8, number: u16, definitions: bool) -> Vec<u8> {
    let mut p = pcs(state, number, 0, &[(7, 0, 0, 1, 2, None)]);
    if definitions {
        p.extend(wds(&[(0, 0, 0, 64, 32)]));
        p.extend(palette());
        p.extend(ods(7, 1, 0xc0, 4, 2, 1, &[1, 2, 0, 0]));
    }
    p.extend(segment(0x80, &[]));
    p
}
fn clock(pts: Option<i64>) -> PacketClock {
    PacketClock {
        pts,
        dts: Some(-19),
        timebase: TimeBase::new(1, 90000).unwrap(),
    }
}
fn owned(b: &Budget, p: &[u8]) -> Bytes {
    Bytes::copy_from_slice(b, p).unwrap()
}
fn limits() -> DisplayLimits {
    DisplayLimits {
        max_segments: 16,
        ..DisplayLimits::default()
    }
}
fn boundary(d: &mut DisplayDecoder, q: u64, c: &AtomicBool) -> Result<DisplayStatus, Error> {
    assert_ne!(q, 0);
    for _ in 0..2_000_000 {
        let before = d.work_spent();
        let mut w = WorkBudget::new(q);
        let status = d.poll(&mut w, c);
        assert_eq!(d.work_spent() - before, w.spent() as usize);
        match status {
            Ok(DisplayStatus::Yield) => {}
            other => return other,
        }
    }
    panic!("display work failed to terminate")
}
fn submit(
    d: &mut DisplayDecoder,
    b: &Budget,
    p: &[u8],
    time: PacketClock,
    q: u64,
) -> Result<Vec<DisplaySet>, Error> {
    let c = AtomicBool::new(false);
    d.push(owned(b, p), time, Envelope::Packet, &c)
        .map_err(|e| e.error)?;
    let mut out = Vec::new();
    loop {
        match boundary(d, q, &c)? {
            DisplayStatus::OutputReady => out.push(d.take_output(&c)?),
            DisplayStatus::NeedPacket => return Ok(out),
            other => panic!("unexpected {other:?}"),
        }
    }
}
fn sticky(d: &mut DisplayDecoder, b: &Budget, error: Error) {
    let c = AtomicBool::new(false);
    assert_eq!(d.poll(&mut WorkBudget::new(0), &c), Err(error));
    assert_eq!(d.finish_input(&c), Err(error));
    assert_eq!(d.take_output(&c).unwrap_err(), error);
    let p = owned(b, &[1]);
    let ptr = p.as_ptr();
    let e = d.push(p, clock(None), Envelope::Packet, &c).unwrap_err();
    assert_eq!(e.error, error);
    assert_eq!(e.packet.as_ptr(), ptr);
    drop(e);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn complete_display_has_literal_source_indices_colour_clocks_and_native_argb_pixels() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let out = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(90001)), 1).unwrap();
    assert_eq!(out.len(), 1);
    let f = &out[0];
    let o = &f.objects()[0];
    assert_eq!(
        (
            f.presentation.width,
            f.presentation.height,
            f.presentation.number
        ),
        (64, 32, 1)
    );
    assert_eq!(f.presentation.stamp.packet_clock, clock(Some(90001)));
    assert_eq!(f.presentation.stamp.packet_clock.pts, Some(90001));
    assert_eq!(f.native_pts_microseconds, Some(1_000_011));
    assert_eq!(f.source_end, None);
    assert_eq!(o.pixels().as_ref(), &[1, 2]);
    assert_eq!(o.visible_pixels().as_ref(), &[1, 2]);
    assert_eq!(
        o.native_argb_pixels().as_ref(),
        &[255, 0, 0, 0, 255, 255, 255, 255]
    );
    let s = o.palette().source(1).unwrap();
    assert_eq!(
        (s.y, s.cr, s.cb, s.alpha, s.palette_version),
        (16, 128, 128, 255, 0)
    );
    assert_eq!(o.palette().source(0), None);
    assert_eq!(
        o.palette().native_origin(0),
        NativeColourOrigin::ReceiverZeroInitialization
    );
    assert_eq!(f.segments().len(), 5);
    assert_eq!((d.cached_objects(), d.cached_palettes()), (1, 1));
    drop(d);
    assert!(b.used() > Budget::control_storage_bytes());
    drop(out);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn normal_cache_reuse_is_retained_and_a_clear_event_does_not_discard_caches() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let a = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 3).unwrap();
    let ptr = a[0].objects()[0].pixels().as_ptr();
    let clear = [pcs(0, 2, 0, &[]), segment(0x80, &[])].concat();
    let c = submit(&mut d, &b, &clear, clock(Some(2)), 7).unwrap();
    assert!(c[0].is_clear());
    assert_eq!((d.cached_objects(), d.cached_palettes()), (1, 1));
    let z = submit(&mut d, &b, &frame(0, 3, false), clock(Some(3)), 4096).unwrap();
    assert_eq!(z[0].objects()[0].pixels().as_ptr(), ptr);
    assert_eq!(z[0].objects()[0].object_version, 1);
    assert_eq!(a[0].objects()[0].pixels().as_ref(), &[1, 2]);
    drop(d);
    drop(c);
    drop(z);
    drop(a);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn partial_palette_updates_keep_prior_entry_versions_and_old_retained_outputs() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let a = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(10)), 1).unwrap();
    let mut p = pcs(0, 2, 0, &[(7, 0, 0, 3, 4, None)]);
    p[11] = 0x80;
    p.extend(pds(0, 9, &[[1, 235, 128, 128, 17], [1, 16, 128, 128, 23]]));
    p.extend(segment(0x80, &[]));
    let z = submit(&mut d, &b, &p, clock(Some(20)), 2).unwrap();
    let pal = z[0].objects()[0].palette();
    assert_eq!(pal.version(), 9);
    assert_eq!(pal.source(1).unwrap().alpha, 23);
    assert_eq!(pal.source(1).unwrap().palette_version, 9);
    assert_eq!(pal.source(2).unwrap().palette_version, 0);
    assert_eq!(pal.source(2).unwrap().stamp.packet_clock.pts, Some(10));
    assert_eq!(pal.source(1).unwrap().stamp.packet_clock.pts, Some(20));
    assert_eq!(a[0].objects()[0].palette().source(1).unwrap().alpha, 255);
    assert_eq!(z[0].presentation.palette_update_flags, 0x80);
    assert_eq!(
        z[0].segments()[1].raw.as_ref(),
        pds(0, 9, &[[1, 235, 128, 128, 17], [1, 16, 128, 128, 23]])
    );
}

#[test]
fn epoch_and_acquisition_invalidate_source_caches_without_fabricating_missing_colours() {
    for state in [0x40, 0x80, 0xc0] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let old = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 7).unwrap();
        let mut p = pcs(state, 2, 3, &[(9, 0, 0, 0, 0, None)]);
        p.extend(pds(3, 1, &[[1, 235, 128, 128, 99]]));
        p.extend(ods(9, 2, 0xc0, 1, 1, 1, &[2]));
        p.extend(segment(0x80, &[]));
        let next = submit(&mut d, &b, &p, clock(Some(2)), 1).unwrap();
        let o = &next[0].objects()[0];
        assert_eq!(next[0].epoch, 2);
        assert_eq!((d.cached_objects(), d.cached_palettes()), (1, 1));
        assert_eq!(o.palette().source(2), None);
        assert_eq!(
            o.palette().native_argb(2),
            old[0].objects()[0].palette().native_argb(2)
        );
        assert_eq!(
            o.palette().native_origin(2),
            NativeColourOrigin::PreviousEpochSlot
        );
        assert_eq!(o.window, None);
        assert!(!o.visibility_window_known);
        assert_eq!(old[0].objects()[0].palette().source(2).unwrap().alpha, 255);
        assert_eq!(
            next[0].presentation.state,
            match state {
                0x40 => CompositionState::Acquisition,
                0x80 => CompositionState::EpochStart,
                _ => CompositionState::ReceiverReset3,
            }
        );
    }
}

#[test]
fn epoch_and_acquisition_cannot_reuse_an_absent_old_object_or_palette() {
    for state in [0x40, 0x80, 0xc0] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        drop(submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 7).unwrap());
        let error = submit(&mut d, &b, &frame(state, 2, false), clock(Some(2)), 1).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
        sticky(&mut d, &b, error);
    }
}

#[test]
fn interleaved_fragmented_objects_and_every_split_survive_all_work_quanta() {
    let encoded = [1, 2, 0, 0, 2, 1, 0, 0];
    for split in 0..=encoded.len() {
        for q in [1, 2, 3, 7, 4096, u64::MAX] {
            let b = Budget::new(2 * 1024 * 1024).unwrap();
            let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
            let start = [
                pcs(
                    0x80,
                    1,
                    0,
                    &[(7, 0, 0, 0, 0, None), (8, 0, 0x40, 4, 0, None)],
                ),
                palette(),
            ]
            .concat();
            assert!(
                submit(&mut d, &b, &start, clock(Some(3)), q)
                    .unwrap()
                    .is_empty()
            );
            for (i, p) in [
                ods(7, 4, 0x80, encoded.len(), 2, 2, &encoded[..split]),
                ods(8, 5, 0x80, 2, 2, 1, &[2]),
                ods(7, 4, 0x40, 0, 0, 0, &encoded[split..]),
                ods(8, 5, 0x40, 0, 0, 0, &[1]),
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    submit(&mut d, &b, &p, clock(Some(4 + i as i64)), q)
                        .unwrap()
                        .is_empty()
                );
            }
            let out = submit(&mut d, &b, &segment(0x80, &[]), clock(Some(9)), q).unwrap();
            assert_eq!(out[0].objects().len(), 2);
            assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2, 2, 1]);
            assert_eq!(out[0].objects()[1].pixels().as_ref(), &[2, 1]);
            assert_eq!(out[0].objects()[0].object_first_stamp.packet_sequence, 1);
            assert_eq!(out[0].objects()[0].object_last_stamp.packet_sequence, 3);
            assert_eq!(out[0].presentation.stamp.packet_clock.pts, Some(3));
            assert_eq!(out[0].end_stamp.packet_clock.pts, Some(9));
            assert_eq!(out[0].native_pts_microseconds, Some(33));
        }
    }
}

#[test]
fn crop_and_window_intersection_materializes_literal_noncontiguous_source_rows() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let pixels = (1..=12).collect::<Vec<_>>();
    let encoded = rle(&pixels, 4);
    let p = [
        pcs(0x80, 1, 0, &[(7, 3, 0xc0, 5, 6, Some((1, 0, 3, 3)))]),
        wds(&[(3, 6, 7, 2, 2)]),
        palette(),
        ods(7, 2, 0xc0, encoded.len(), 4, 3, &encoded),
        segment(0x80, &[]),
    ]
    .concat();
    let out = submit(&mut d, &b, &p, clock(Some(1)), 1).unwrap();
    let o = &out[0].objects()[0];
    assert_eq!(o.pixels().as_ref(), pixels);
    assert_eq!(o.visible_pixels().as_ref(), &[7, 8, 11, 12]);
    assert_eq!(
        (
            o.visible.x,
            o.visible.y,
            o.visible.width,
            o.visible.height,
            o.visible.source_x,
            o.visible.source_y
        ),
        (6, 7, 2, 2, 2, 1)
    );
    assert!(o.composition.forced());
    assert_eq!(o.composition.crop.unwrap().x, 1);
    assert_eq!((o.native_x, o.native_y), (5, 6));
    assert_eq!(o.native_argb_pixels().len(), pixels.len() * 4);
}

#[test]
fn window_replacement_preserves_unknown_window_facts_and_does_not_default_to_window_zero() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    drop(submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 4096).unwrap());
    let p = [
        pcs(0, 2, 0, &[(7, 0, 0, 1, 2, None)]),
        wds(&[(9, 0, 0, 10, 10)]),
        segment(0x80, &[]),
    ]
    .concat();
    let out = submit(&mut d, &b, &p, clock(Some(2)), 1).unwrap();
    assert_eq!(out[0].objects()[0].window, None);
    assert!(!out[0].objects()[0].visibility_window_known);
    assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn repeated_object_references_preserve_order_and_per_reference_forced_and_crop_facts() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    drop(submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 4096).unwrap());
    let p = [
        pcs(
            0,
            2,
            0,
            &[
                (7, 0, 0x80, 9, 1, Some((1, 0, 1, 1))),
                (7, 0, 0x40, 2, 3, None),
            ],
        ),
        segment(0x80, &[]),
    ]
    .concat();
    let out = submit(&mut d, &b, &p, clock(Some(2)), 3).unwrap();
    let a = &out[0].objects()[0];
    let z = &out[0].objects()[1];
    assert_eq!((a.composition.x, z.composition.x), (9, 2));
    assert_eq!(a.visible_pixels().as_ref(), &[2]);
    assert!(!a.composition.forced());
    assert!(z.composition.forced());
    assert!(a.pixels().shares_storage_with(z.pixels()));
}

#[test]
fn transparent_rgb_and_forced_only_native_suppression_never_change_source_colours() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(
        &b,
        limits(),
        DisplayConfig {
            native_forced_only: true,
        },
    )
    .unwrap();
    let p = [
        pcs(
            0x80,
            1,
            0,
            &[(7, 0, 0, 0, 0, None), (7, 0, 0x40, 0, 1, None)],
        ),
        palette(),
        ods(7, 1, 0xc0, 1, 1, 1, &[3]),
        segment(0x80, &[]),
    ]
    .concat();
    let out = submit(&mut d, &b, &p, clock(None), 1).unwrap();
    let a = &out[0].objects()[0];
    let z = &out[0].objects()[1];
    assert!(a.native_forced_only_suppressed);
    assert_eq!(a.native_argb_pixels().as_ref(), &[0; 4]);
    assert!(!z.native_forced_only_suppressed);
    assert_eq!(z.native_argb_pixels().as_ref(), &[0, 255, 255, 255]);
    assert_eq!(a.palette().source(3).unwrap().alpha, 0);
    assert_eq!(a.palette().native_argb(3), 0x00ff_ffff);
}

#[test]
fn matrix_choice_is_a_receiver_fact_at_palette_update_not_invented_source_metadata() {
    assert_eq!(NativeMatrix::Sd601.argb(16, 128, 128, 127), 0x7f00_0000);
    assert_eq!(NativeMatrix::Hd709.argb(235, 128, 128, 255), u32::MAX);
    assert_eq!(NativeMatrix::Hd709.argb(235, 128, 128, 0), 0x00ff_ffff);
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let old = submit(&mut d, &b, &frame(0x80, 1, true), clock(None), 7).unwrap();
    assert_eq!(
        old[0].objects()[0]
            .palette()
            .source(4)
            .unwrap()
            .receiver_matrix,
        NativeMatrix::Sd601
    );
    let mut p = pcs(0, 2, 0, &[(7, 0, 0, 0, 0, None)]);
    p[5..7].copy_from_slice(&1080u16.to_be_bytes());
    p.extend(pds(0, 1, &[[1, 81, 240, 90, 255]]));
    p.extend(segment(0x80, &[]));
    let z = submit(&mut d, &b, &p, clock(None), 1).unwrap();
    assert_eq!(
        z[0].objects()[0]
            .palette()
            .source(1)
            .unwrap()
            .receiver_matrix,
        NativeMatrix::Hd709
    );
    assert_eq!(
        z[0].objects()[0]
            .palette()
            .source(4)
            .unwrap()
            .receiver_matrix,
        NativeMatrix::Sd601
    );
    assert_eq!(
        z[0].objects()[0].palette().native_argb(4),
        old[0].objects()[0].palette().native_argb(4)
    );
}

#[test]
fn exact_negative_large_and_prime_packet_clocks_are_retained_before_native_rounding() {
    for (ticks, n, dn) in [
        (-90001, 1, 90000),
        (9_007_199_254_740_993, 1, 1_000_000),
        (17, 19, 1001),
    ] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let time = PacketClock {
            pts: Some(ticks),
            dts: Some(ticks - 1),
            timebase: TimeBase::new(n, dn).unwrap(),
        };
        let out = submit(&mut d, &b, &frame(0x80, 1, true), time, 1).unwrap();
        assert_eq!(out[0].presentation.stamp.packet_clock, time);
        assert_eq!(
            out[0].presentation.stamp.packet_clock.pts_time().unwrap(),
            Some(Time::from_ticks(i128::from(ticks), time.timebase).unwrap())
        );
        assert_eq!(
            out[0].native_pts_microseconds,
            time.native_microseconds().unwrap()
        );
        assert_eq!(out[0].native_clock_source, NativeClockSource::Presentation);
    }
}

#[test]
fn end_fallback_is_labelled_and_never_fills_a_missing_source_presentation_timestamp() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let mut start = frame(0x80, 1, true);
    start.truncate(start.len() - 3);
    assert!(
        submit(&mut d, &b, &start, clock(None), 1)
            .unwrap()
            .is_empty()
    );
    let out = submit(&mut d, &b, &segment(0x80, &[]), clock(Some(90000)), 1).unwrap();
    assert_eq!(out[0].native_pts_microseconds, Some(1_000_000));
    assert_eq!(out[0].native_clock_source, NativeClockSource::EndFallback);
    assert_eq!(out[0].presentation.stamp.packet_clock.pts, None);
    assert_eq!(out[0].end_stamp.packet_clock.pts, Some(90000));
    assert_eq!(out[0].source_end, None);
}

#[test]
fn native_unrepresentable_clocks_remain_explicit_while_source_pixels_and_clock_survive() {
    for time in [
        PacketClock {
            pts: Some(i64::MIN),
            dts: None,
            timebase: TimeBase::new(1, 1_000_000).unwrap(),
        },
        PacketClock {
            pts: Some(i64::MAX),
            dts: None,
            timebase: TimeBase::new(u64::MAX, 1).unwrap(),
        },
    ] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let out = submit(&mut d, &b, &frame(0x80, 1, true), time, 4096).unwrap();
        assert_eq!(out[0].presentation.stamp.packet_clock, time);
        assert_eq!(out[0].native_pts_microseconds, None);
        assert!(out[0].native_time_error.is_some());
        assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2]);
    }
}

#[test]
fn sup_raw_clocks_cross_wrap_without_an_invented_epoch_or_absolute_unwrap() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let c = AtomicBool::new(false);
    for (index, pts) in [u32::MAX - 15, 3].into_iter().enumerate() {
        let packet = frame(if index == 0 { 0x80 } else { 0 }, index as u16, index == 0);
        let mut sup = Vec::new();
        let mut at = 0;
        while at < packet.len() {
            let length = 3 + usize::from(u16::from_be_bytes([packet[at + 1], packet[at + 2]]));
            sup.extend_from_slice(b"PG");
            sup.extend_from_slice(&pts.to_be_bytes());
            sup.extend_from_slice(&(pts.wrapping_sub(2)).to_be_bytes());
            sup.extend_from_slice(&packet[at..at + length]);
            at += length;
        }
        d.push(owned(&b, &sup), clock(None), Envelope::Sup, &c)
            .unwrap();
        let mut result = None;
        loop {
            match boundary(&mut d, 1, &c).unwrap() {
                DisplayStatus::OutputReady => result = Some(d.take_output(&c).unwrap()),
                DisplayStatus::NeedPacket => break,
                _ => panic!(),
            }
        }
        let out = result.unwrap();
        assert_eq!(out.presentation.stamp.packet_clock.pts, None);
        assert_eq!(out.presentation.stamp.sup_clock.unwrap().pts, pts);
        assert_eq!(
            out.presentation.stamp.sup_clock.unwrap().dts,
            pts.wrapping_sub(2)
        );
        assert_eq!(out.epoch, 1);
    }
}

#[test]
fn unknown_segment_bytes_stay_observable_in_the_retained_display_trace() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let mut p = frame(0x80, 1, true);
    p.truncate(p.len() - 3);
    let opaque = segment(0x99, &[1, 2, 3, 4, 5]);
    p.extend(&opaque);
    p.extend(segment(0x80, &[]));
    let out = submit(&mut d, &b, &p, clock(None), 1).unwrap();
    let unknown = out[0].segments().iter().find(|s| s.kind == 0x99).unwrap();
    assert_eq!(unknown.raw.as_ref(), opaque);
    drop(d);
    assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn zero_work_cannot_allocate_parse_copy_or_publish_new_state() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let c = AtomicBool::new(false);
    let baseline = b.used();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &c).unwrap(),
        DisplayStatus::Yield
    );
    assert_eq!(b.used(), baseline);
    d.push(
        owned(&b, &frame(0x80, 1, true)),
        clock(None),
        Envelope::Packet,
        &c,
    )
    .unwrap();
    for _ in 0..100_000 {
        let before = (
            b.used(),
            d.work_spent(),
            d.cached_objects(),
            d.cached_palettes(),
        );
        for _ in 0..3 {
            assert_eq!(
                d.poll(&mut WorkBudget::new(0), &c).unwrap(),
                DisplayStatus::Yield
            );
            assert_eq!(
                (
                    b.used(),
                    d.work_spent(),
                    d.cached_objects(),
                    d.cached_palettes()
                ),
                before
            );
        }
        match d.poll(&mut WorkBudget::new(1), &c).unwrap() {
            DisplayStatus::Yield => (),
            DisplayStatus::OutputReady => break,
            status => panic!("unexpected {status:?}"),
        }
    }
    assert_eq!(boundary(&mut d, 1, &c).unwrap(), DisplayStatus::OutputReady);
    let before = b.used();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &c).unwrap(),
        DisplayStatus::OutputReady
    );
    assert_eq!(b.used(), before);
}

#[test]
fn every_work_quantum_has_equal_literal_output_and_exact_total_work() {
    let mut expected = None;
    for q in [1, 2, 3, 7, 17, 4096, u64::MAX] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let out = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), q).unwrap();
        assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2]);
        assert_eq!(
            out[0].objects()[0].native_argb_pixels().as_ref(),
            &[255, 0, 0, 0, 255, 255, 255, 255]
        );
        match expected {
            None => expected = Some(d.work_spent()),
            Some(value) => assert_eq!(d.work_spent(), value),
        }
    }
    let work = expected.unwrap();
    for maximum in [work - 1, work] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(
            &b,
            DisplayLimits {
                max_work: maximum,
                ..limits()
            },
            DisplayConfig::default(),
        )
        .unwrap();
        let result = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 1);
        if maximum == work {
            assert_eq!(result.unwrap().len(), 1);
        } else {
            let e = result.unwrap_err();
            assert!(matches!(e, Error::Limit { .. }));
            sticky(&mut d, &b, e);
        }
    }
}

#[test]
fn initial_allocation_pressure_is_retryable_without_recharging_setup() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let c = AtomicBool::new(false);
    let blocker = b.reserve(b.available()).unwrap();
    assert_eq!(
        d.poll(&mut WorkBudget::new(1), &c).unwrap(),
        DisplayStatus::Yield
    );
    assert_eq!(d.work_spent(), 1);
    for _ in 0..3 {
        assert!(matches!(
            d.poll(&mut WorkBudget::new(1), &c),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(d.work_spent(), 1);
    }
    drop(blocker);
    let before = b.used();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &c).unwrap(),
        DisplayStatus::Yield
    );
    assert_eq!(b.used(), before);
    assert_eq!(boundary(&mut d, 1, &c).unwrap(), DisplayStatus::NeedPacket);
    drop(d);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn pressure_at_every_allocation_and_freeze_restores_state_and_does_not_repeat_work() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let c = AtomicBool::new(false);
    d.push(
        owned(&b, &frame(0x80, 1, true)),
        clock(None),
        Envelope::Packet,
        &c,
    )
    .unwrap();
    let mut pressure = 0;
    let mut output = None;
    for _ in 0..20_000 {
        let blocker = b.reserve(b.available()).unwrap();
        let result = d.poll(&mut WorkBudget::new(1), &c);
        match result {
            Err(Error::Backpressure { .. }) => {
                pressure += 1;
                let before = (b.used(), d.work_spent());
                for _ in 0..2 {
                    assert!(matches!(
                        d.poll(&mut WorkBudget::new(1), &c),
                        Err(Error::Backpressure { .. })
                    ));
                    assert_eq!((b.used(), d.work_spent()), before);
                }
                drop(blocker);
                let before = (b.used(), d.work_spent());
                assert_eq!(
                    d.poll(&mut WorkBudget::new(0), &c).unwrap(),
                    DisplayStatus::Yield
                );
                assert_eq!((b.used(), d.work_spent()), before);
                d.poll(&mut WorkBudget::new(1), &c).unwrap();
            }
            Ok(DisplayStatus::OutputReady) => {
                drop(blocker);
                output = Some(d.take_output(&c).unwrap());
                break;
            }
            Ok(DisplayStatus::Yield) => drop(blocker),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        pressure >= 10,
        "expected allocation and owner-freeze sites: {pressure}"
    );
    let out = output.unwrap();
    assert_eq!(out.objects()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(
        out.objects()[0].native_argb_pixels().as_ref(),
        &[255, 0, 0, 0, 255, 255, 255, 255]
    );
}

#[test]
fn caller_retained_old_display_causes_real_pool_pressure_and_release_allows_exact_retry() {
    let b = Budget::new(96 * 1024).unwrap();
    let config = DisplayLimits {
        max_objects: 2,
        max_palettes: 1,
        max_segments: 8,
        ..limits()
    };
    let mut d = DisplayDecoder::new(&b, config, DisplayConfig::default()).unwrap();
    let pixels = vec![1; 4096];
    let encoded = rle(&pixels, 64);
    let first = [
        pcs(0x80, 1, 0, &[(7, 0, 0, 0, 0, None)]),
        palette(),
        ods(7, 1, 0xc0, encoded.len(), 64, 64, &encoded),
        segment(0x80, &[]),
    ]
    .concat();
    let held = submit(&mut d, &b, &first, clock(Some(1)), 4096).unwrap();
    let second = [
        pcs(0, 2, 0, &[(7, 0, 0, 0, 0, None)]),
        pds(0, 1, &[[1, 235, 128, 128, 255]]),
        segment(0x80, &[]),
    ]
    .concat();
    let c = AtomicBool::new(false);
    d.push(owned(&b, &second), clock(Some(2)), Envelope::Packet, &c)
        .unwrap();
    // Storage is smaller now. Force genuine pool contention while keeping this
    // reservation alive: releasing the old display alone must enable retry.
    let _pressure = b
        .reserve((96 * 1024 - b.used()).saturating_sub(8192))
        .unwrap();
    let error = boundary(&mut d, 4096, &c).unwrap_err();
    assert!(matches!(error, Error::Backpressure { .. }));
    assert_eq!(held[0].objects()[0].pixels().as_ref(), pixels);
    assert_eq!(
        held[0].objects()[0].native_argb_pixels()[0..4],
        [255, 0, 0, 0]
    );
    let before = d.work_spent();
    drop(held);
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &c).unwrap(),
        DisplayStatus::Yield
    );
    assert_eq!(d.work_spent(), before);
    assert_eq!(
        boundary(&mut d, 4096, &c).unwrap(),
        DisplayStatus::OutputReady
    );
    let out = d.take_output(&c).unwrap();
    assert_eq!(
        out.objects()[0].native_argb_pixels()[0..4],
        [255, 255, 255, 255]
    );
}

#[test]
fn cancellation_at_many_phases_is_sticky_and_hard_reset_is_explicit_recovery() {
    for steps in [0, 1, 70, 350, 700, 1100, 1600, 10_000] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let c = AtomicBool::new(false);
        d.push(
            owned(&b, &frame(0x80, 1, true)),
            clock(Some(1)),
            Envelope::Packet,
            &c,
        )
        .unwrap();
        for _ in 0..steps {
            if d.poll(&mut WorkBudget::new(1), &c).unwrap() == DisplayStatus::OutputReady {
                break;
            }
        }
        c.store(true, Ordering::Relaxed);
        assert_eq!(d.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
        c.store(false, Ordering::Relaxed);
        sticky(&mut d, &b, Error::Cancelled);
        d.reset(&c).unwrap();
        assert_eq!(d.generation(), 1);
        let out = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(2)), 3).unwrap();
        assert_eq!(out[0].generation, 1);
        assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2]);
    }
}

#[test]
fn hard_reset_releases_cache_owners_but_keeps_prior_returned_display_unchanged() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let old = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 7).unwrap();
    let c = AtomicBool::new(false);
    d.reset(&c).unwrap();
    assert_eq!((d.cached_objects(), d.cached_palettes()), (0, 0));
    let new = submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(2)), 1).unwrap();
    assert_eq!(old[0].generation, 0);
    assert_eq!(new[0].generation, 1);
    assert_eq!(old[0].objects()[0].pixels().as_ref(), &[1, 2]);
    drop(d);
    drop(new);
    assert!(b.used() > Budget::control_storage_bytes());
    drop(old);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn busy_push_returns_exact_owner_and_foreign_pool_input_is_sticky() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let c = AtomicBool::new(false);
    d.push(
        owned(&b, &frame(0x80, 1, true)),
        clock(Some(1)),
        Envelope::Packet,
        &c,
    )
    .unwrap();
    let p = owned(&b, &frame(0, 2, false));
    let ptr = p.as_ptr();
    let e = d.push(p, clock(Some(2)), Envelope::Packet, &c).unwrap_err();
    assert_eq!(
        e.error,
        Error::Backpressure {
            requested: 1,
            available: 0
        }
    );
    assert_eq!(e.packet.as_ptr(), ptr);
    drop(e);
    let other = Budget::new(4096).unwrap();
    let p = owned(&other, &[1]);
    let ptr = p.as_ptr();
    let e = d.push(p, clock(None), Envelope::Packet, &c).unwrap_err();
    assert_eq!(e.error, Error::ForeignPool);
    assert_eq!(e.packet.as_ptr(), ptr);
    drop(e);
    sticky(&mut d, &b, Error::ForeignPool);
    assert_eq!(other.used(), Budget::control_storage_bytes());
}

#[test]
fn packet_reference_object_palette_segment_and_render_bounds_are_real_limits() {
    for config in [
        DisplayLimits {
            max_packet_bytes: 1,
            ..limits()
        },
        DisplayLimits {
            max_axis: 31,
            ..limits()
        },
        DisplayLimits {
            max_canvas_pixels: 1,
            ..limits()
        },
        DisplayLimits {
            max_object_pixels: 1,
            ..limits()
        },
        DisplayLimits {
            max_encoded: 1,
            ..limits()
        },
        DisplayLimits {
            max_segments: 1,
            ..limits()
        },
        DisplayLimits {
            max_references: 0,
            ..limits()
        },
        DisplayLimits {
            max_display_pixels: 1,
            ..limits()
        },
    ] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, config, DisplayConfig::default()).unwrap();
        let error = submit(&mut d, &b, &frame(0x80, 1, true), clock(None), 1).unwrap_err();
        assert!(matches!(error, Error::Limit { .. }), "{error:?}");
        sticky(&mut d, &b, error);
    }
    for palettes in [false, true] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(
            &b,
            DisplayLimits {
                max_objects: 1,
                max_palettes: 1,
                ..limits()
            },
            DisplayConfig::default(),
        )
        .unwrap();
        let extra = if palettes {
            pds(1, 0, &[[1, 16, 128, 128, 255]])
        } else {
            ods(8, 1, 0xc0, 1, 1, 1, &[1])
        };
        let mut p = frame(0x80, 1, true);
        p.truncate(p.len() - 3);
        p.extend(extra);
        p.extend(segment(0x80, &[]));
        let e = submit(&mut d, &b, &p, clock(None), 1).unwrap_err();
        assert!(matches!(e, Error::Limit { .. }));
        sticky(&mut d, &b, e);
    }
}

#[test]
fn truncated_malformed_reserved_and_incomplete_display_sets_fail_without_publishing() {
    let mut cases = Vec::new();
    for end in 1..14 {
        cases.push(frame(0x80, 1, true)[..end].to_vec());
    }
    cases.push([pcs(0x81, 1, 0, &[]), segment(0x80, &[])].concat());
    cases.push(
        [
            pcs(0x80, 1, 0, &[(7, 0, 1, 0, 0, None)]),
            segment(0x80, &[]),
        ]
        .concat(),
    );
    cases.push(
        [
            pcs(0x80, 1, 0, &[]),
            segment(0x17, &[1]),
            segment(0x80, &[]),
        ]
        .concat(),
    );
    cases.push(
        [
            pcs(0x80, 1, 0, &[]),
            segment(0x14, &[0, 0, 1]),
            segment(0x80, &[]),
        ]
        .concat(),
    );
    cases.push([pcs(0x80, 1, 0, &[]), segment(0x80, &[1])].concat());
    cases.push(segment(0x80, &[]));
    for p in cases {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let result = submit(&mut d, &b, &p, clock(None), 1);
        let error = match result {
            Err(e) => e,
            Ok(out) => {
                assert!(out.is_empty());
                d.finish_input(&AtomicBool::new(false)).unwrap_err()
            }
        };
        assert!(matches!(
            error,
            Error::Truncated | Error::Invalid(_) | Error::Unsupported(_)
        ));
        sticky(&mut d, &b, error);
    }
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let mut p = frame(0x80, 1, true);
    p.truncate(p.len() - 3);
    assert!(submit(&mut d, &b, &p, clock(None), 1).unwrap().is_empty());
    assert_eq!(
        d.finish_input(&AtomicBool::new(false)),
        Err(Error::Truncated)
    );
    sticky(&mut d, &b, Error::Truncated);
}

#[test]
fn missing_last_and_wrong_object_fragment_identity_are_never_hidden_by_cached_images() {
    for bad in [
        ods(7, 1, 0x80, 2, 2, 1, &[1]),
        ods(7, 1, 0xc0, 2, 2, 1, &[1]),
        ods(7, 1, 0xc0, 1, 1, 1, &[1, 2]),
    ] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        drop(submit(&mut d, &b, &frame(0x80, 1, true), clock(Some(1)), 7).unwrap());
        let p = [
            pcs(0, 2, 0, &[(7, 0, 0, 0, 0, None)]),
            bad,
            segment(0x80, &[]),
        ]
        .concat();
        let e = submit(&mut d, &b, &p, clock(Some(2)), 1).unwrap_err();
        assert!(matches!(e, Error::Invalid(_) | Error::Truncated));
        sticky(&mut d, &b, e);
    }
    for version in [2, 3] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let p = [
            pcs(0x80, 1, 0, &[(7, 0, 0, 0, 0, None)]),
            palette(),
            ods(7, 1, 0x80, 2, 2, 1, &[1]),
            ods(7, version, 0x40, 0, 0, 0, &[2]),
            segment(0x80, &[]),
        ]
        .concat();
        let e = submit(&mut d, &b, &p, clock(None), 1).unwrap_err();
        assert!(matches!(e, Error::Invalid(_)));
        sticky(&mut d, &b, e);
    }
}

#[test]
fn multiple_display_sets_in_one_source_packet_remain_distinct_and_in_wire_order() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let p = [
        frame(0x80, 7, true),
        frame(0, 3, false),
        [pcs(0, 9, 0, &[]), segment(0x80, &[])].concat(),
    ]
    .concat();
    let out = submit(&mut d, &b, &p, clock(Some(90000)), 3).unwrap();
    assert_eq!(
        out.iter()
            .map(|o| o.presentation.number)
            .collect::<Vec<_>>(),
        [7, 3, 9]
    );
    assert!(out[2].is_clear());
    assert_eq!(
        out[0].presentation.stamp.packet_sequence,
        out[1].presentation.stamp.packet_sequence
    );
}

#[test]
fn explicit_eof_finishes_after_the_last_output_without_inventing_a_final_clear_or_end_time() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let out = submit(&mut d, &b, &frame(0x80, 1, true), clock(None), 1).unwrap();
    let c = AtomicBool::new(false);
    d.finish_input(&c).unwrap();
    assert_eq!(boundary(&mut d, 1, &c).unwrap(), DisplayStatus::Ended);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].native_pts_microseconds, None);
    assert_eq!(out[0].native_clock_source, NativeClockSource::Unknown);
    assert_eq!(out[0].source_end, None);
    assert!(!out[0].is_clear());
}

#[test]
fn unused_fragmented_object_survives_clear_ends_until_a_later_normal_display_uses_it() {
    for clear_count in [1, 2, 3] {
        let b = Budget::new(2 * 1024 * 1024).unwrap();
        let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
        let first = [
            pcs(0x80, 1, 0, &[]),
            palette(),
            ods(7, 4, 0x80, 2, 2, 1, &[1]),
            segment(0x80, &[]),
        ]
        .concat();
        let mut held = submit(&mut d, &b, &first, clock(Some(90000)), 1).unwrap();
        assert!(held[0].is_clear());
        assert_eq!(d.cached_objects(), 0);
        for number in 2..=clear_count {
            let clear = [pcs(0, number, 0, &[]), segment(0x80, &[])].concat();
            let output = submit(&mut d, &b, &clear, clock(Some(180000)), 3).unwrap();
            assert!(output[0].is_clear());
            held.extend(output);
        }
        let last = [
            pcs(0, clear_count + 1, 0, &[(7, 0, 0, 0, 0, None)]),
            ods(7, 4, 0x40, 0, 0, 0, &[2]),
            segment(0x80, &[]),
        ]
        .concat();
        let output = submit(&mut d, &b, &last, clock(Some(270000)), 1).unwrap();
        let object = &output[0].objects()[0];
        assert_eq!(object.pixels().as_ref(), &[1, 2]);
        assert_eq!(object.object_version, 4);
        assert_eq!(object.object_first_stamp.packet_sequence, 0);
        assert_eq!(object.object_first_stamp.packet_clock.pts, Some(90000));
        assert_eq!(
            object.object_last_stamp.packet_sequence,
            u64::from(clear_count)
        );
        assert_eq!(d.cached_objects(), 1);
        drop(d);
        drop(output);
        drop(held);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn epoch_reset_discards_unused_incomplete_objects_without_invalidating_prior_clear_output() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let first = [
        pcs(0x80, 1, 0, &[]),
        ods(7, 0, 0x80, 2, 2, 1, &[1]),
        segment(0x80, &[]),
    ]
    .concat();
    let old = submit(&mut d, &b, &first, clock(Some(1)), 1).unwrap();
    let next = submit(
        &mut d,
        &b,
        &[pcs(0x80, 2, 0, &[]), segment(0x80, &[])].concat(),
        clock(Some(2)),
        1,
    )
    .unwrap();
    assert!(old[0].is_clear());
    assert!(next[0].is_clear());
    assert_eq!(old[0].epoch, 1);
    assert_eq!(next[0].epoch, 2);
    assert_eq!(old[0].segments()[1].kind, 0x15);
    let cancel = AtomicBool::new(false);
    d.finish_input(&cancel).unwrap();
    assert_eq!(boundary(&mut d, 1, &cancel).unwrap(), DisplayStatus::Ended);
    drop(d);
    drop(next);
    drop(old);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn incomplete_unused_object_at_explicit_eof_is_a_sticky_truncation_after_valid_clear_output() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let first = [
        pcs(0x80, 1, 0, &[]),
        ods(7, 0, 0x80, 2, 2, 1, &[1]),
        segment(0x80, &[]),
    ]
    .concat();
    let out = submit(&mut d, &b, &first, clock(Some(1)), 1).unwrap();
    assert!(out[0].is_clear());
    let cancel = AtomicBool::new(false);
    d.finish_input(&cancel).unwrap();
    assert_eq!(boundary(&mut d, 1, &cancel), Err(Error::Truncated));
    assert_eq!(out[0].presentation.stamp.packet_clock.pts, Some(1));
    drop(out);
    sticky(&mut d, &b, Error::Truncated);
}

#[test]
fn complete_unreferenced_object_is_preloaded_and_reused_without_another_ods() {
    let b = Budget::new(2 * 1024 * 1024).unwrap();
    let mut d = DisplayDecoder::new(&b, limits(), DisplayConfig::default()).unwrap();
    let first = [
        pcs(0x80, 1, 0, &[]),
        palette(),
        ods(7, 6, 0xc0, 2, 2, 1, &[1, 2]),
        segment(0x80, &[]),
    ]
    .concat();
    let old = submit(&mut d, &b, &first, clock(Some(1)), 1).unwrap();
    assert!(old[0].is_clear());
    assert_eq!(d.cached_objects(), 1);
    let out = submit(&mut d, &b, &frame(0, 2, false), clock(Some(2)), 1).unwrap();
    assert_eq!(out[0].objects()[0].object_version, 6);
    assert_eq!(out[0].objects()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(out[0].objects()[0].object_first_stamp.packet_sequence, 0);
}
