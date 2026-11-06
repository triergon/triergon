use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, TimeBase, WorkBudget};
use triergon_subtitles::compat::mov_text::{Clock, Decoder, Limits, Options, Output, Status};
use triergon_subtitles::text::{Encoding, Format, Input, Timing};

fn input(pool: &Budget, bytes: &[u8]) -> Input {
    Input {
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: Bytes::copy_from_slice(pool, bytes).unwrap(),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(77),
        generation: 6,
        sequence: 4,
        configuration_epoch: 3,
        container_position: None,
    }
}
fn context(pool: &Budget) -> Decoder {
    Decoder::new(pool, None, Options::default(), Limits::default(), 6).unwrap()
}
fn finish(decoder: &mut Decoder, quantum: u64) -> Output {
    let cancel = AtomicBool::new(false);
    for _ in 0..2_000_000 {
        match decoder
            .poll(&mut WorkBudget::new(quantum), &cancel)
            .unwrap()
        {
            Status::Pending => (),
            Status::OutputReady => return decoder.take_output(&cancel).unwrap(),
            s => panic!("unexpected native status {s:?}"),
        }
    }
    panic!("native progress bound")
}
fn run(bytes: &[u8], quantum: u64) -> Output {
    let pool = Budget::new(16 * 1024 * 1024).unwrap();
    let mut d = context(&pool);
    d.provide(input(&pool, bytes), Clock::default()).unwrap();
    finish(&mut d, quantum)
}
fn sample(text: &[u8], boxes: &[u8]) -> Vec<u8> {
    let mut b = u16::try_from(text.len()).unwrap().to_be_bytes().to_vec();
    b.extend(text);
    b.extend(boxes);
    b
}
fn box_bytes(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    b.extend(kind);
    b.extend(body);
    b
}

#[test]
fn native_plain_literal_and_source_identity_at_work_one_seven_and_large() {
    for q in [1, 7, 4096] {
        let out = run(&sample("Å水e\u{301}😀".as_bytes(), &[]), q);
        assert_eq!(
            out.subtitle.as_ref().unwrap().ass.as_ref(),
            "0,0,Default,,0,0,0,,Å水e\u{301}😀".as_bytes()
        );
        assert_eq!(
            (
                out.input.source_id,
                out.input.generation,
                out.input.sequence,
                out.input.configuration_epoch
            ),
            (SourceId(77), 6, 4, 3)
        );
        assert_eq!(
            out.input.payload.as_ref(),
            sample("Å水e\u{301}😀".as_bytes(), &[])
        );
    }
}
#[test]
fn native_newlines_and_ass_markup_are_distinct_from_original_source() {
    let bytes = sample(b"A\r\n{\\b1}B\\NC\n", &[]);
    let out = run(&bytes, 1);
    assert_eq!(
        out.subtitle.unwrap().ass.as_ref(),
        b"0,0,Default,,0,0,0,,A\\N{\\b1}B\\NC\\N"
    );
    assert_eq!(out.input.payload.as_ref(), bytes);
}
#[test]
fn native_nul_boundary_retains_whole_source_and_ignores_invalid_suffix_only_in_view() {
    let bytes = sample(b"a\0\xffb", &[]);
    let out = run(&bytes, 1);
    assert_eq!(out.subtitle.unwrap().ass.as_ref(), b"0,0,Default,,0,0,0,,a");
    assert_eq!(out.input.payload.as_ref(), bytes);
}
#[test]
fn utf16_and_invalid_utf8_are_explicit_native_refusals_without_rewriting_source() {
    for b in [
        &[0, 4, 0xfe, 0xff, 0, 65][..],
        &[0, 1, 0xff][..],
        &[0, 3, 0xef, 0xbf, 0xbe][..],
    ] {
        let out = run(b, 1);
        assert_eq!(out.returned, -1094995529);
        assert!(out.subtitle.is_none());
        assert_eq!(out.input.payload.as_ref(), b);
    }
}
#[test]
fn clear_empty_and_short_invalid_calls_do_not_fabricate_subtitles() {
    for (b, rc) in [
        (&[][..], 0),
        (&[0, 0][..], 0),
        (&[0][..], -1094995529),
        (&[0, 1][..], -1094995529),
    ] {
        let out = run(b, 1);
        assert_eq!(out.returned, rc);
        assert!(out.subtitle.is_none());
    }
}
#[test]
fn native_length_clamp_and_bad_box_refusal_leave_source_grammar_unchanged() {
    let out = run(&[0, 9, b'A'], 1);
    assert_eq!(out.subtitle.unwrap().ass.as_ref(), b"0,0,Default,,0,0,0,,A");
    let bad = sample(b"a", &[0, 0, 0, 0, b't', b'w', b'r', b'p']);
    assert_eq!(run(&bad, 1).returned, -1094995529);
}
#[test]
fn wrap_and_unknown_modifier_are_native_selected_views_of_complete_owned_input() {
    let mut boxes = box_bytes(b"twrp", &[0]);
    boxes.extend(box_bytes(b"zzzz", b"kept\0bytes"));
    boxes.extend(box_bytes(b"twrp", &[1]));
    let bytes = sample(b"ab", &boxes);
    let out = run(&bytes, 1);
    assert_eq!(
        out.subtitle.unwrap().ass.as_ref(),
        b"0,0,Default,,0,0,0,,{\\q1}ab"
    );
    assert_eq!(out.input.payload.as_ref(), bytes);
}
#[test]
fn style_ranges_count_unicode_scalars_and_reset_before_next_character() {
    let body = [0, 1, 0, 1, 0, 2, 0, 0, 1, 0, 0, 0, 0, 0];
    let out = run(&sample("Å水z".as_bytes(), &box_bytes(b"styl", &body)), 1);
    assert_eq!(
        out.subtitle.unwrap().ass.as_ref(),
        "0,0,Default,,0,0,0,,Å{\\b1}水{\\r}z".as_bytes()
    );
}
#[test]
fn read_order_tracks_success_post_decode_utf8_failure_and_pre_decode_failure() {
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    for (bytes, expected) in [
        (vec![0], None),
        (vec![0, 0], None),
        (vec![0, 1, 255], None),
        (sample(b"x", &[]), Some(&b"1,0,Default,,0,0,0,,x"[..])),
    ] {
        d.provide(input(&pool, &bytes), Clock::default()).unwrap();
        let o = finish(&mut d, 1);
        if let Some(expected) = expected {
            assert_eq!(o.subtitle.unwrap().ass.as_ref(), expected);
        } else {
            assert!(o.subtitle.is_none());
        }
    }
}
#[test]
fn unset_context_clock_never_invents_time_from_exact_source_timing() {
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    let mut i = input(&pool, &sample(b"a", &[]));
    i.timing = Timing::from_ticks(triergon_subtitles::text::RawClock {
        numerator: 1,
        denominator: 1000,
        pts_ticks: Some(-5),
        dts_ticks: Some(-6),
        duration_ticks: Some(1001),
    })
    .unwrap();
    let original = i.timing;
    d.provide(
        i,
        Clock {
            pts: Some(-5),
            dts: Some(-6),
            duration: Some(1001),
        },
    )
    .unwrap();
    let o = finish(&mut d, 1);
    assert_eq!(o.input.timing, original);
    let s = o.subtitle.unwrap();
    assert_eq!(s.pts_microseconds, None);
    assert_eq!(s.end_display_ms, 0);
}
#[test]
fn known_clock_uses_nearest_away_and_explicit_native_duration_wrapping() {
    let pool = Budget::new(65536).unwrap();
    let options = Options {
        packet_timebase: Some(TimeBase::new(1, 2000).unwrap()),
        ..Options::default()
    };
    let mut d = Decoder::new(&pool, None, options, Limits::default(), 6).unwrap();
    d.provide(
        input(&pool, &sample(b"a", &[])),
        Clock {
            pts: Some(-1),
            dts: None,
            duration: Some(-1),
        },
    )
    .unwrap();
    let s = finish(&mut d, 1).subtitle.unwrap();
    assert_eq!(s.pts_microseconds, Some(-500));
    assert_eq!(s.end_display_ms, u32::MAX);
    assert_eq!(
        (
            s.format(),
            s.rectangle_type(),
            s.geometry(),
            s.flags(),
            s.palette_entries()
        ),
        (1, 3, [0; 4], 0, 0)
    );
}
#[test]
fn work_zero_and_cancel_preserve_cursors_funding_and_allocation_state() {
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    d.provide(input(&pool, &sample(b"a", &[])), Clock::default())
        .unwrap();
    let cancel = AtomicBool::new(false);
    let used = pool.used();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        Status::Pending
    );
    assert_eq!(d.work_spent(), 0);
    assert_eq!(pool.used(), used);
    d.poll(&mut WorkBudget::new(1), &cancel).unwrap();
    let spent = d.work_spent();
    let used = pool.used();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        d.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(d.work_spent(), spent);
    assert_eq!(pool.used(), used);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        finish(&mut d, 1).subtitle.unwrap().ass.as_ref(),
        b"0,0,Default,,0,0,0,,a"
    );
}
#[test]
fn pressure_resumes_without_recharging_a_fully_funded_allocation() {
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    d.provide(input(&pool, &sample(b"a", &[])), Clock::default())
        .unwrap();
    let block = pool.reserve(pool.available()).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        d.poll(&mut WorkBudget::new(1_000_000), &cancel).unwrap(),
        Status::NeedOutput
    );
    let spent = d.work_spent();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        Status::Pending
    );
    assert_eq!(d.work_spent(), spent);
    drop(block);
    let o = finish(&mut d, 1);
    assert!(o.subtitle.is_some());
    let total = d.work_spent();
    let other = Budget::new(65536).unwrap();
    let mut fresh = context(&other);
    fresh
        .provide(input(&other, &sample(b"a", &[])), Clock::default())
        .unwrap();
    finish(&mut fresh, 1);
    assert_eq!(total, fresh.work_spent());
}
#[test]
fn foreign_pool_busy_and_stale_generation_rejections_return_exact_input() {
    let pool = Budget::new(65536).unwrap();
    let foreign = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    let e = d
        .provide(input(&foreign, b"foreign"), Clock::default())
        .unwrap_err();
    assert_eq!(e.error, Error::ForeignPool);
    assert_eq!(e.input.payload.as_ref(), b"foreign");
    let mut i = input(&pool, b"stale");
    i.generation = 5;
    let e = d.provide(i, Clock::default()).unwrap_err();
    assert_eq!(e.input.generation, 5);
    d.provide(input(&pool, &sample(b"a", &[])), Clock::default())
        .unwrap();
    let e = d
        .provide(input(&pool, b"busy"), Clock::default())
        .unwrap_err();
    assert_eq!(e.input.payload.as_ref(), b"busy");
}
#[test]
fn take_cancel_reset_and_context_drop_preserve_retained_output_owners() {
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    d.provide(input(&pool, &sample(b"a", &[])), Clock::default())
        .unwrap();
    let cancel = AtomicBool::new(false);
    while d.poll(&mut WorkBudget::new(4096), &cancel).unwrap() != Status::OutputReady {}
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(d.take_output(&cancel), Err(Error::Cancelled)));
    cancel.store(false, Ordering::Relaxed);
    let old = d.take_output(&cancel).unwrap();
    d.reset(7, &cancel).unwrap();
    drop(d);
    assert_eq!(old.input.payload.as_ref(), &sample(b"a", &[]));
    assert_eq!(
        old.subtitle.as_ref().unwrap().ass.as_ref(),
        b"0,0,Default,,0,0,0,,a"
    );
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(old);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn core_limits_are_distinct_from_native_data_refusal_and_remain_sticky() {
    let pool = Budget::new(65536).unwrap();
    let limits = Limits {
        maximum_ass_bytes: 4,
        ..Limits::default()
    };
    let mut d = Decoder::new(&pool, None, Options::default(), limits, 6).unwrap();
    d.provide(input(&pool, &sample(b"a", &[])), Clock::default())
        .unwrap();
    let cancel = AtomicBool::new(false);
    let e = loop {
        match d.poll(&mut WorkBudget::new(4096), &cancel) {
            Err(e) => break e,
            Ok(Status::Pending) => (),
            s => panic!("{s:?}"),
        }
    };
    assert!(matches!(e, Error::Limit { maximum: 4, .. }));
    assert_eq!(d.poll(&mut WorkBudget::new(4096), &cancel), Err(e));
    drop(d);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn eof_has_no_delayed_ass_and_never_accepts_an_extra_packet() {
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    let cancel = AtomicBool::new(false);
    d.finish_input(&cancel).unwrap();
    loop {
        match d.poll(&mut WorkBudget::new(4096), &cancel).unwrap() {
            Status::Pending => (),
            Status::Finished => break,
            s => panic!("{s:?}"),
        }
    }
    let e = d
        .provide(input(&pool, &sample(b"x", &[])), Clock::default())
        .unwrap_err();
    assert_eq!(e.input.payload.as_ref(), sample(b"x", &[]));
}
#[test]
fn complete_u16_text_length_is_not_clamped_to_the_2048_recommendation() {
    let text = vec![b'x'; 65535];
    let bytes = sample(&text, &[]);
    let o = run(&bytes, 4096);
    let s = o.subtitle.unwrap();
    let prefix = b"0,0,Default,,0,0,0,,";
    assert_eq!(s.ass.len(), prefix.len() + 65535);
    assert_eq!(&s.ass.as_ref()[prefix.len()..], text);
    assert_eq!(o.input.payload.as_ref(), bytes);
}

#[path = "native-mov-text-fixtures/cases.rs"]
mod captured;

#[test]
fn configuration_backing_and_header_remain_owned_after_decoder_drop() {
    let c = captured::CASES
        .iter()
        .find(|c| c.id == "configured-defaults")
        .unwrap();
    let pool = Budget::new(256 * 1024).unwrap();
    let raw = c.config.unwrap();
    let mut backing = vec![0; 100_000];
    backing[100..100 + raw.len()].copy_from_slice(raw);
    let backing = Bytes::copy_from_slice(&pool, &backing).unwrap();
    let extra = backing.slice(100..100 + raw.len()).unwrap();
    drop(backing);
    let mut d = Decoder::new(
        &pool,
        Some(&extra),
        Options::default(),
        Limits::default(),
        6,
    )
    .unwrap();
    let mut i = input(&pool, &sample(b"x", &[]));
    i.codec_private = Some(extra.clone());
    d.provide(i, Clock::default()).unwrap();
    let out = finish(&mut d, 1);
    let header = d.subtitle_header().unwrap().clone();
    let header_copy = header.as_ref().to_vec();
    drop(extra);
    drop(d);
    assert_eq!(out.input.codec_private.as_ref().unwrap().as_ref(), raw);
    assert!(pool.used() >= 100_000);
    drop(out);
    assert_eq!(header.as_ref(), header_copy);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(header);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn native_configuration_and_style_count_limits_are_explicit_core_errors() {
    let pool = Budget::new(65536).unwrap();
    let c = captured::CASES
        .iter()
        .find(|c| c.id == "configured-defaults")
        .unwrap();
    let config = Bytes::copy_from_slice(&pool, c.config.unwrap()).unwrap();
    let limits = Limits {
        maximum_fonts: 0,
        ..Limits::default()
    };
    let mut d = Decoder::new(&pool, Some(&config), Options::default(), limits, 6).unwrap();
    assert!(matches!(
        d.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Limit { maximum: 0, .. })
    ));
    let limits = Limits {
        maximum_styles: 0,
        ..Limits::default()
    };
    let mut d = Decoder::new(&pool, None, Options::default(), limits, 6).unwrap();
    let body = [0, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0];
    d.provide(
        input(&pool, &sample(b"x", &box_bytes(b"styl", &body))),
        Clock::default(),
    )
    .unwrap();
    loop {
        match d.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)) {
            Err(Error::Limit { maximum: 0, .. }) => break,
            Ok(Status::Pending) => (),
            other => panic!("policy error was replaced by {other:?}"),
        }
    }
}

#[test]
fn cancelling_after_configuration_prefix_keeps_exact_font_source_and_header() {
    let c = captured::CASES
        .iter()
        .find(|c| c.id == "configured-defaults")
        .unwrap();
    let pool = Budget::new(65536).unwrap();
    let extra = Bytes::copy_from_slice(&pool, c.config.unwrap()).unwrap();
    let options = Options {
        frame_width: c.dimensions.0,
        frame_height: c.dimensions.1,
        ..Options::default()
    };
    let mut d = Decoder::new(&pool, Some(&extra), options, Limits::default(), 6).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        d.poll(&mut WorkBudget::new(40), &cancel).unwrap(),
        Status::Pending
    );
    let spent = d.work_spent();
    let used = pool.used();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        d.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!((d.work_spent(), pool.used()), (spent, used));
    cancel.store(false, Ordering::Relaxed);
    loop {
        match d.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
            Status::Pending => (),
            Status::NeedInput => break,
            s => panic!("{s:?}"),
        }
    }
    assert_eq!(d.subtitle_header().unwrap().as_ref(), c.header);
    assert_eq!(d.extradata().unwrap().as_ref(), c.config.unwrap());
}

#[test]
fn cancellation_at_each_funded_emission_step_never_duplicates_style_output() {
    let c = captured::CASES
        .iter()
        .find(|c| c.id == "style-all")
        .unwrap();
    let captured::Op::Packet {
        raw,
        subtitle: Some(expected),
        ..
    } = &c.ops[0]
    else {
        panic!("fixture")
    };
    let pool = Budget::new(65536).unwrap();
    let mut d = context(&pool);
    d.provide(input(&pool, raw), Clock::default()).unwrap();
    let cancel = AtomicBool::new(false);
    loop {
        let status = d.poll(&mut WorkBudget::new(1), &cancel).unwrap();
        if status == Status::OutputReady {
            break;
        }
        let spent = d.work_spent();
        let used = pool.used();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            d.poll(&mut WorkBudget::new(4096), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!((d.work_spent(), pool.used()), (spent, used));
        cancel.store(false, Ordering::Relaxed);
    }
    assert_eq!(
        d.take_output(&cancel)
            .unwrap()
            .subtitle
            .unwrap()
            .ass
            .as_ref(),
        expected.ass
    );
}

fn check_native_capture(c: &captured::Case) {
    for q in [1, 7, 4096] {
        println!("native-capture-start {} quantum={}", c.id, q);
        let pool = Budget::new(32 * 1024 * 1024).unwrap();
        let extra = c.config.map(|b| Bytes::copy_from_slice(&pool, b).unwrap());
        let options = Options {
            packet_timebase: c.timebase.map(|(n, d)| TimeBase::new(n, d).unwrap()),
            frame_width: c.dimensions.0,
            frame_height: c.dimensions.1,
        };
        let mut d = Decoder::new(&pool, extra.as_ref(), options, Limits::default(), 6).unwrap();
        let cancel = AtomicBool::new(false);
        loop {
            match d.poll(&mut WorkBudget::new(q), &cancel).unwrap() {
                Status::Pending => (),
                Status::NeedInput => break,
                s => panic!("{} opening {s:?}", c.id),
            }
        }
        assert_eq!(
            d.subtitle_header().unwrap().as_ref(),
            c.header,
            "{} header",
            c.id
        );
        for op in c.ops {
            match op {
                captured::Op::Flush => d.reset(6, &cancel).unwrap(),
                captured::Op::Packet {
                    raw,
                    returned,
                    subtitle,
                } => {
                    let mut i = input(&pool, raw);
                    i.codec_private.clone_from(&extra);
                    let clock = Clock {
                        pts: c.pts,
                        dts: None,
                        duration: c.duration,
                    };
                    d.provide(i, clock).unwrap();
                    let out = finish(&mut d, q);
                    assert_eq!(out.input.payload.as_ref(), *raw, "{} retained packet", c.id);
                    assert_eq!(out.clock, clock, "{} raw native clock", c.id);
                    assert_eq!(out.returned, *returned, "{} native return", c.id);
                    match (out.subtitle, subtitle) {
                        (None, None) => (),
                        (Some(s), Some(e)) => {
                            assert_eq!(s.ass.as_ref(), e.ass, "{} complete ASS", c.id);
                            assert_eq!(
                                s.pts_microseconds.unwrap_or(i64::MIN),
                                e.pts,
                                "{} pts",
                                c.id
                            );
                            assert_eq!(
                                (s.format(), s.start_display_ms, s.end_display_ms),
                                (e.format, e.start, e.end),
                                "{} subtitle",
                                c.id
                            );
                            assert_eq!(
                                (
                                    s.rectangle_type(),
                                    s.geometry(),
                                    s.flags(),
                                    s.palette_entries()
                                ),
                                (e.kind, e.xywh, e.flags, e.colors),
                                "{} rectangle",
                                c.id
                            );
                        }
                        (actual, expected) => panic!(
                            "{} output presence actual={actual:?} expected={expected:?}",
                            c.id
                        ),
                    }
                }
            }
        }
        d.finish_input(&cancel).unwrap();
        assert_eq!(
            d.poll(&mut WorkBudget::new(q), &cancel).unwrap(),
            Status::Finished
        );
        drop(d);
        drop(extra);
        assert_eq!(
            pool.used(),
            Budget::control_storage_bytes(),
            "{} complete teardown",
            c.id
        );
        println!(
            "native-capture-pass {} quantum={} calls={}",
            c.id,
            q,
            c.ops.len()
        );
    }
}
#[test]
fn captured_original177_tx3g_basic() {
    check_native_capture(&captured::CASES[0]);
}

#[test]
fn captured_original177_tx3g_byte_not_character_count() {
    check_native_capture(&captured::CASES[1]);
}

#[test]
fn captured_original177_tx3g_utf16be_bom() {
    check_native_capture(&captured::CASES[2]);
}

#[test]
fn captured_original177_tx3g_empty_sample() {
    check_native_capture(&captured::CASES[3]);
}

#[test]
fn captured_original177_tx3g_embedded_nul() {
    check_native_capture(&captured::CASES[4]);
}

#[test]
fn captured_original177_tx3g_line_codepoints_exact() {
    check_native_capture(&captured::CASES[5]);
}

#[test]
fn captured_original177_tx3g_style_highlight_raw() {
    check_native_capture(&captured::CASES[6]);
}

#[test]
fn captured_original177_tx3g_unknown_modifier() {
    check_native_capture(&captured::CASES[7]);
}

#[test]
fn captured_original177_tx3g_modifier_order() {
    check_native_capture(&captured::CASES[8]);
}

#[test]
fn captured_original177_tx3g_empty_with_modifier() {
    check_native_capture(&captured::CASES[9]);
}

#[test]
fn captured_original177_tx3g_u16_byte_count_boundary() {
    check_native_capture(&captured::CASES[10]);
}

#[test]
fn captured_original177_tx3g_missing_length() {
    check_native_capture(&captured::CASES[11]);
}

#[test]
fn captured_original177_tx3g_text_overrun() {
    check_native_capture(&captured::CASES[12]);
}

#[test]
fn captured_original177_tx3g_utf16_odd() {
    check_native_capture(&captured::CASES[13]);
}

#[test]
fn captured_original177_tx3g_lone_surrogate() {
    check_native_capture(&captured::CASES[14]);
}

#[test]
fn captured_original177_tx3g_short_box() {
    check_native_capture(&captured::CASES[15]);
}

#[test]
fn captured_original177_tx3g_box_overrun() {
    check_native_capture(&captured::CASES[16]);
}

#[test]
fn captured_original177_tx3g_extended_box_gap() {
    check_native_capture(&captured::CASES[17]);
}

#[test]
fn captured_unicode() {
    check_native_capture(&captured::CASES[18]);
}

#[test]
fn captured_bom() {
    check_native_capture(&captured::CASES[19]);
}

#[test]
fn captured_ffff() {
    check_native_capture(&captured::CASES[20]);
}

#[test]
fn captured_noncharacter_fffe() {
    check_native_capture(&captured::CASES[21]);
}

#[test]
fn captured_invalid_overlong() {
    check_native_capture(&captured::CASES[22]);
}

#[test]
fn captured_invalid_tail() {
    check_native_capture(&captured::CASES[23]);
}

#[test]
fn captured_nul_invalid_tail() {
    check_native_capture(&captured::CASES[24]);
}

#[test]
fn captured_cr_lf_markup() {
    check_native_capture(&captured::CASES[25]);
}

#[test]
fn captured_utf16be() {
    check_native_capture(&captured::CASES[26]);
}

#[test]
fn captured_utf16le() {
    check_native_capture(&captured::CASES[27]);
}

#[test]
fn captured_empty_physical() {
    check_native_capture(&captured::CASES[28]);
}

#[test]
fn captured_one_byte() {
    check_native_capture(&captured::CASES[29]);
}

#[test]
fn captured_clear() {
    check_native_capture(&captured::CASES[30]);
}

#[test]
fn captured_bad_two() {
    check_native_capture(&captured::CASES[31]);
}

#[test]
fn captured_empty_event() {
    check_native_capture(&captured::CASES[32]);
}

#[test]
fn captured_overstated() {
    check_native_capture(&captured::CASES[33]);
}

#[test]
fn captured_understated() {
    check_native_capture(&captured::CASES[34]);
}

#[test]
fn captured_style_all() {
    check_native_capture(&captured::CASES[35]);
}

#[test]
fn captured_style_unicode() {
    check_native_capture(&captured::CASES[36]);
}

#[test]
fn captured_style_empty() {
    check_native_capture(&captured::CASES[37]);
}

#[test]
fn captured_style_adjacent_equal() {
    check_native_capture(&captured::CASES[38]);
}

#[test]
fn captured_style_adjacent_different() {
    check_native_capture(&captured::CASES[39]);
}

#[test]
fn captured_style_overlap() {
    check_native_capture(&captured::CASES[40]);
}

#[test]
fn captured_style_reversed() {
    check_native_capture(&captured::CASES[41]);
}

#[test]
fn captured_style_outside() {
    check_native_capture(&captured::CASES[42]);
}

#[test]
fn captured_style_repeated() {
    check_native_capture(&captured::CASES[43]);
}

#[test]
fn captured_style_malformed_after_good() {
    check_native_capture(&captured::CASES[44]);
}

#[test]
fn captured_style_zero_after_good() {
    check_native_capture(&captured::CASES[45]);
}

#[test]
fn captured_highlight_inverse() {
    check_native_capture(&captured::CASES[46]);
}

#[test]
fn captured_highlight_color() {
    check_native_capture(&captured::CASES[47]);
}

#[test]
fn captured_highlight_empty() {
    check_native_capture(&captured::CASES[48]);
}

#[test]
fn captured_wrap_0() {
    check_native_capture(&captured::CASES[49]);
}

#[test]
fn captured_wrap_1() {
    check_native_capture(&captured::CASES[50]);
}

#[test]
fn captured_wrap_2() {
    check_native_capture(&captured::CASES[51]);
}

#[test]
fn captured_duplicate_wrap() {
    check_native_capture(&captured::CASES[52]);
}

#[test]
fn captured_unknown() {
    check_native_capture(&captured::CASES[53]);
}

#[test]
fn captured_extended_wrap() {
    check_native_capture(&captured::CASES[54]);
}

#[test]
fn captured_size0() {
    check_native_capture(&captured::CASES[55]);
}

#[test]
fn captured_short_box() {
    check_native_capture(&captured::CASES[56]);
}

#[test]
fn captured_overrun_box() {
    check_native_capture(&captured::CASES[57]);
}

#[test]
fn captured_short_body() {
    check_native_capture(&captured::CASES[58]);
}

#[test]
fn captured_extended_short() {
    check_native_capture(&captured::CASES[59]);
}

#[test]
fn captured_wrap_nul() {
    check_native_capture(&captured::CASES[60]);
}

#[test]
fn captured_configured_defaults() {
    check_native_capture(&captured::CASES[61]);
}

#[test]
fn captured_configured_style_font() {
    check_native_capture(&captured::CASES[62]);
}

#[test]
fn captured_duplicate_fonts() {
    check_native_capture(&captured::CASES[63]);
}

#[test]
fn captured_font_nul() {
    check_native_capture(&captured::CASES[64]);
}

#[test]
fn captured_font_utf16() {
    check_native_capture(&captured::CASES[65]);
}

#[test]
fn captured_config_too_short() {
    check_native_capture(&captured::CASES[66]);
}

#[test]
fn captured_config_valid_empty_fonts() {
    check_native_capture(&captured::CASES[67]);
}

#[test]
fn captured_config_invalid_font_size() {
    check_native_capture(&captured::CASES[68]);
}

#[test]
fn captured_config_one_dimension() {
    check_native_capture(&captured::CASES[69]);
}

#[test]
fn captured_unset_clock() {
    check_native_capture(&captured::CASES[70]);
}

#[test]
fn captured_unknown_clock() {
    check_native_capture(&captured::CASES[71]);
}

#[test]
fn captured_known_zero_duration() {
    check_native_capture(&captured::CASES[72]);
}

#[test]
fn captured_positive_half() {
    check_native_capture(&captured::CASES[73]);
}

#[test]
fn captured_negative_half() {
    check_native_capture(&captured::CASES[74]);
}

#[test]
fn captured_rational_clock() {
    check_native_capture(&captured::CASES[75]);
}

#[test]
fn captured_overflow_clock() {
    check_native_capture(&captured::CASES[76]);
}

#[test]
fn captured_two_packets() {
    check_native_capture(&captured::CASES[77]);
}

#[test]
fn captured_utf8_refusal_then_good() {
    check_native_capture(&captured::CASES[78]);
}

#[test]
fn captured_box_refusal_then_good() {
    check_native_capture(&captured::CASES[79]);
}

#[test]
fn captured_clear_then_good() {
    check_native_capture(&captured::CASES[80]);
}

#[test]
fn captured_flush_read_order() {
    check_native_capture(&captured::CASES[81]);
}
