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

// The captured inputs; the reference's recorded outputs beside them are
// checked by the subtitle worker's `subtitle-text` corpus.
#[allow(dead_code)]
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

// The override tags written where a style record or the highlight starts or
// ends, each expectation the reference's output for the same session.

/// A TS 26.245 StyleRecord: characters `start..end`, font ID, face flags (bold
/// 1, italic 2, underline 4), size and RGBA text colour.
#[derive(Clone, Copy)]
struct Record {
    start: u16,
    end: u16,
    font: u16,
    face: u8,
    size: u8,
    rgb: [u8; 3],
    alpha: u8,
}
/// The default style the sample descriptions below declare.
const PLAIN: Record = Record {
    start: 0,
    end: 0,
    font: 1,
    face: 0,
    size: 16,
    rgb: [255; 3],
    alpha: 255,
};
const RED: [u8; 3] = [0x11, 0x22, 0x33];
const GREEN: [u8; 3] = [0x44, 0x55, 0x66];
fn at(start: u16, end: u16) -> Record {
    Record {
        start,
        end,
        ..PLAIN
    }
}
impl Record {
    fn bytes(self) -> Vec<u8> {
        let mut b: Vec<u8> = [self.start, self.end, self.font]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        b.extend([self.face, self.size]);
        b.extend(self.rgb);
        b.push(self.alpha);
        b
    }
}
fn styl(records: &[Record]) -> Vec<u8> {
    let mut body = u16::try_from(records.len()).unwrap().to_be_bytes().to_vec();
    body.extend(records.iter().flat_map(|r| r.bytes()));
    box_bytes(b"styl", &body)
}
fn hlit(start: u16, end: u16) -> Vec<u8> {
    box_bytes(b"hlit", &[start.to_be_bytes(), end.to_be_bytes()].concat())
}
fn styled(text: &[u8], records: &[Record], boxes: &[Vec<u8>]) -> Vec<u8> {
    let mut all = styl(records);
    all.extend(boxes.concat());
    sample(text, &all)
}
/// A TextSampleEntry body: display flags, justification, background, text
/// box, the default style and the font table.
fn description(default: Record, fonts: &[(u16, &[u8])]) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0, 0, 0xff, 0, 0, 0, 0xff];
    b.extend([0; 8]);
    b.extend(default.bytes());
    let mut table = u16::try_from(fonts.len()).unwrap().to_be_bytes().to_vec();
    for (id, name) in fonts {
        table.extend(id.to_be_bytes());
        table.push(u8::try_from(name.len()).unwrap());
        table.extend(*name);
    }
    b.extend(box_bytes(b"ftab", &table));
    b
}
/// Decodes `samples` in one session at work quanta 1 and 4096, which must
/// agree, and returns each call's ASS text after its prefix, which must carry
/// the sample's index as its read order; `None` for a refusal.
fn session(config: Option<&[u8]>, samples: &[Vec<u8>]) -> Vec<Option<String>> {
    let decode = |quantum| {
        let pool = Budget::new(1 << 20).unwrap();
        let extra = config.map(|c| Bytes::copy_from_slice(&pool, c).unwrap());
        let options = Options::default();
        let mut d = Decoder::new(&pool, extra.as_ref(), options, Limits::default(), 6).unwrap();
        let mut texts = Vec::new();
        for (read, bytes) in samples.iter().enumerate() {
            d.provide(input(&pool, bytes), Clock::default()).unwrap();
            let out = finish(&mut d, quantum);
            let refused = out.subtitle.is_none();
            texts.push(out.subtitle.map(|s| {
                let ass = String::from_utf8(s.ass.as_ref().to_vec()).unwrap();
                let prefix = format!("{read},0,Default,,0,0,0,,");
                ass.strip_prefix(&prefix).unwrap().to_owned()
            }));
            if refused {
                assert_eq!(out.returned, -1094995529);
            }
        }
        texts
    };
    let texts = decode(1);
    assert_eq!(texts, decode(4096));
    texts
}
fn expect(config: Option<&[u8]>, samples: &[Vec<u8>], expected: &[Option<&str>]) {
    let expected: Vec<_> = expected.iter().map(|e| e.map(str::to_owned)).collect();
    assert_eq!(session(config, samples), expected);
}

#[test]
fn face_tags_are_written_for_each_flag_that_differs_from_the_default() {
    let bold = description(Record { face: 1, ..PLAIN }, &[(1, b"Arial")]);
    let mut samples: Vec<_> = [0, 2, 4, 6, 7]
        .map(|face| styled(b"abcdef", &[Record { face, ..at(1, 3) }], &[]))
        .to_vec();
    samples.push(styled(
        b"abcdef",
        &[Record {
            face: 1,
            size: 20,
            ..at(1, 3)
        }],
        &[],
    ));
    expect(
        Some(&bold),
        &samples,
        &[
            Some(r"a{\b0}bc{\r}def"),
            Some(r"a{\b0}{\i1}bc{\r}def"),
            Some(r"a{\b0}{\u1}bc{\r}def"),
            Some(r"a{\b0}{\i1}{\u1}bc{\r}def"),
            Some(r"a{\i1}{\u1}bc{\r}def"),
            Some(r"a{\fs20}bc{\r}def"),
        ],
    );
    let all = description(Record { face: 7, ..PLAIN }, &[(1, b"Arial")]);
    let samples = [0, 3, 5].map(|face| styled(b"abcdef", &[Record { face, ..at(1, 3) }], &[]));
    expect(
        Some(&all),
        &samples,
        &[
            Some(r"a{\b0}{\i0}{\u0}bc{\r}def"),
            Some(r"a{\u0}bc{\r}def"),
            Some(r"a{\i0}bc{\r}def"),
        ],
    );
}

#[test]
fn size_colour_and_alpha_tags_follow_in_order_with_their_own_digits() {
    let plain = description(PLAIN, &[(1, b"Arial"), (2, b"Two")]);
    let records = [
        Record {
            size: 0,
            ..at(1, 3)
        },
        Record {
            rgb: [0x0a, 0, 0],
            ..at(1, 3)
        },
        Record {
            rgb: [0, 0, 0x0a],
            ..at(1, 3)
        },
        Record {
            alpha: 0xfe,
            ..at(1, 3)
        },
        Record {
            alpha: 0xf0,
            ..at(1, 3)
        },
        Record {
            font: 2,
            face: 7,
            size: 30,
            rgb: RED,
            alpha: 0x10,
            ..at(1, 3)
        },
    ];
    let samples = records.map(|r| styled(b"abcdef", &[r], &[]));
    expect(
        Some(&plain),
        &samples,
        &[
            Some(r"a{\fs0}bc{\r}def"),
            Some(r"a{\1c&HA&}bc{\r}def"),
            Some(r"a{\1c&HA0000&}bc{\r}def"),
            Some(r"a{\1a&H01&}bc{\r}def"),
            Some(r"a{\1a&H0F&}bc{\r}def"),
            Some(r"a{\b1}{\i1}{\u1}{\fs30}{\fnTwo}{\1c&H332211&}{\1a&HEF&}bc{\r}def"),
        ],
    );
    // Without a sample description the default style is all zeros.
    let record = Record {
        font: 3,
        face: 1,
        ..at(1, 3)
    };
    expect(
        None,
        &[styled(b"abcdef", &[record], &[])],
        &[Some(r"a{\b1}{\fs16}{\1c&HFFFFFF&}{\1a&H00&}bc{\r}def")],
    );
}

#[test]
fn font_tags_name_every_table_entry_with_the_id_and_refuse_names_that_are_not_text() {
    let fonts: &[(u16, &[u8])] = &[
        (1, b"Arial"),
        (2, b"Two"),
        (3, b""),
        (2, b"Deux"),
        (5, b"X\0Y"),
        (6, b"\xff\xfe"),
        (7, b"A\xef\xbf\xbe"),
        (8, b"A\xef\xbf\xbf"),
    ];
    let config = description(PLAIN, fonts);
    let font = |font| styled(b"abcdef", &[Record { font, ..at(1, 3) }], &[]);
    let samples = [
        font(2),
        font(3),
        font(5),
        font(9),
        font(6),
        sample(b"abcdef", &[]),
        font(7),
        font(8),
    ];
    expect(
        Some(&config),
        &samples,
        &[
            Some(r"a{\fnTwo}{\fnDeux}bc{\r}def"),
            Some(r"a{\fn}bc{\r}def"),
            Some(r"a{\fnX}bc{\r}def"),
            Some(r"abc{\r}def"),
            None,
            Some("abcdef"),
            None,
            Some("a{\\fnA\u{ffff}}bc{\\r}def"),
        ],
    );
}

#[test]
fn font_lookups_cost_work_in_the_entries_written_not_the_table_size() {
    // 16,384 entries in descending ID order, and 1,000 style starts, each
    // with a font ID one entry has.
    let names: Vec<(u16, &[u8])> = (2..=16_385u16).rev().map(|id| (id, &b"f"[..])).collect();
    let mut fonts = vec![(1, &b"Arial"[..])];
    fonts.extend(names);
    let config = description(PLAIN, &fonts);
    let records: Vec<_> = (0..1000u16)
        .map(|i| Record {
            font: 2 + 16 * i,
            ..at(2 * i, 2 * i + 1)
        })
        .collect();
    let bytes = styled(&[b'a'; 2000], &records, &[]);
    let pool = Budget::new(4 << 20).unwrap();
    let extra = Bytes::copy_from_slice(&pool, &config).unwrap();
    let mut d = Decoder::new(
        &pool,
        Some(&extra),
        Options::default(),
        Limits::default(),
        6,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    while d.poll(&mut WorkBudget::new(1 << 20), &cancel).unwrap() != Status::NeedInput {}
    let configured = d.work_spent();
    d.provide(input(&pool, &bytes), Clock::default()).unwrap();
    let ass = finish(&mut d, 1 << 20).subtitle.unwrap().ass;
    let text = String::from_utf8(ass.as_ref().to_vec()).unwrap();
    assert_eq!(text.matches(r"{\fnf}").count(), 1000);
    assert!(text.ends_with(r"{\fnf}a{\r}a"));
    // Linear in the sample and its output: each style start finds its entry
    // by binary search instead of examining all 16,384.
    let spent = d.work_spent() - configured;
    assert!(spent < 200_000, "{spent} units for the sample");
}

#[test]
fn font_tags_longer_than_the_pending_buffer_are_written_in_stages() {
    let names: Vec<_> = (0..24u8).map(|i| vec![b'A' + i; 200]).collect();
    let mut fonts: Vec<(u16, &[u8])> = vec![(1, b"Arial")];
    fonts.extend(names.iter().map(|n| (7, n.as_slice())));
    let config = description(PLAIN, &fonts);
    let texts = session(
        Some(&config),
        &[styled(
            b"abcdef",
            &[Record {
                font: 7,
                ..at(1, 3)
            }],
            &[],
        )],
    );
    let tags: String = names
        .iter()
        .map(|n| format!("{{\\fn{}}}", String::from_utf8_lossy(n)))
        .collect();
    assert_eq!(texts, [Some(format!("a{tags}bc{{\\r}}def"))]);
}

#[test]
fn a_highlight_end_restores_the_colour_of_the_style_in_effect() {
    let plain = description(PLAIN, &[(1, b"Arial")]);
    let colour = box_bytes(b"hclr", &[0xaa, 0xbb, 0xcc, 0xff]);
    let red = |start, end| Record {
        rgb: RED,
        ..at(start, end)
    };
    let green = |start, end| Record {
        rgb: GREEN,
        ..at(start, end)
    };
    let samples = [
        styled(b"abcd", &[red(0, 4)], &[hlit(1, 2)]),
        styled(b"abcd", &[red(0, 4)], &[hlit(1, 2), colour.clone()]),
        styled(b"abcd", &[red(0, 2)], &[hlit(1, 3)]),
        styled(b"abcd", &[red(0, 2), green(2, 4)], &[hlit(1, 3)]),
        sample(b"abcd", &[hlit(3, 1), colour].concat()),
        sample(b"abcd", &hlit(3, 1)),
    ];
    expect(
        Some(&plain),
        &samples,
        &[
            Some(r"{\1c&H332211&}a{\1c&H000000&}{\2c&HFFFFFF&}b{\1c&H332211&}{\2c&HFFFFFF&}cd"),
            Some(r"{\1c&H332211&}a{\2c&Hccbbaa&}b{\2c&HFFFFFF&}cd"),
            Some(r"{\1c&H332211&}a{\1c&H000000&}{\2c&HFFFFFF&}b{\r}c{\1c&HFFFFFF&}{\2c&HFFFFFF&}d"),
            Some(concat!(
                r"{\1c&H332211&}a{\1c&H000000&}{\2c&HFFFFFF&}b{\r}",
                r"{\1c&H665544&}c{\1c&H665544&}{\2c&HFFFFFF&}d"
            )),
            Some(r"a{\2c&HFFFFFF&}bc{\2c&Hccbbaa&}d"),
            Some(r"a{\1c&HFFFFFF&}{\2c&HFFFFFF&}bc{\1c&H000000&}{\2c&HFFFFFF&}d"),
        ],
    );
    // The colour restored is the default's, and a highlight colour keeps six
    // digits.
    let config = description(
        Record {
            rgb: RED,
            alpha: 0x40,
            ..PLAIN
        },
        &[(1, b"Arial")],
    );
    let samples = [
        sample(b"abcd", &hlit(1, 2)),
        sample(
            b"abcd",
            &[hlit(1, 2), box_bytes(b"hclr", &[1, 2, 3, 4])].concat(),
        ),
    ];
    expect(
        Some(&config),
        &samples,
        &[
            Some(r"a{\1c&H000000&}{\2c&HFFFFFF&}b{\1c&H332211&}{\2c&H332211&}cd"),
            Some(r"a{\2c&H030201&}b{\2c&H332211&}cd"),
        ],
    );
}

#[test]
fn a_style_end_and_start_precede_the_highlight_start_and_end_at_one_character() {
    let plain = description(PLAIN, &[(1, b"Arial")]);
    let red = |start, end| Record {
        rgb: RED,
        ..at(start, end)
    };
    let samples = [
        styled(
            b"abcd",
            &[Record {
                face: 1,
                ..red(1, 3)
            }],
            &[hlit(1, 3)],
        ),
        styled(b"abcd", &[red(2, 4)], &[hlit(0, 2)]),
        styled(
            b"abcd",
            &[
                red(0, 2),
                Record {
                    rgb: GREEN,
                    ..at(2, 4)
                },
            ],
            &[hlit(2, 3)],
        ),
        styled(
            b"abcd",
            &[Record {
                face: 1,
                ..at(1, 2)
            }],
            &[hlit(2, 2)],
        ),
    ];
    expect(
        Some(&plain),
        &samples,
        &[
            Some(concat!(
                r"a{\b1}{\1c&H332211&}{\1c&H000000&}{\2c&HFFFFFF&}bc{\r}",
                r"{\1c&HFFFFFF&}{\2c&HFFFFFF&}d"
            )),
            Some(r"{\1c&H000000&}{\2c&HFFFFFF&}ab{\1c&H332211&}{\1c&H332211&}{\2c&HFFFFFF&}cd"),
            Some(concat!(
                r"{\1c&H332211&}ab{\r}{\1c&H665544&}{\1c&H000000&}{\2c&HFFFFFF&}c",
                r"{\1c&H665544&}{\2c&HFFFFFF&}d"
            )),
            Some(r"a{\b1}b{\r}{\1c&H000000&}{\2c&HFFFFFF&}{\1c&HFFFFFF&}{\2c&HFFFFFF&}cd"),
        ],
    );
}

#[test]
fn transitions_at_a_nul_are_written_and_those_at_the_end_of_the_text_are_not() {
    let config = description(PLAIN, &[(1, b"Arial"), (6, b"\xff\xfe")]);
    let bold = |start, end| Record {
        face: 1,
        ..at(start, end)
    };
    let invalid = Record {
        font: 6,
        ..at(2, 3)
    };
    let samples = [
        styled(b"ab\0cd", &[bold(0, 2)], &[]),
        styled(b"ab\0cd", &[bold(2, 3)], &[]),
        sample(b"ab\0cd", &hlit(2, 2)),
        styled(b"ab\0cd", &[bold(3, 4)], &[]),
        styled(b"\0ab", &[bold(0, 2)], &[]),
        styled(b"ab\0\xffd", &[bold(2, 3)], &[]),
        styled(b"ab", &[bold(0, 2)], &[hlit(2, 2)]),
        styled(b"ab", &[invalid], &[]),
        styled(b"ab\0cd", &[invalid], &[]),
    ];
    expect(
        Some(&config),
        &samples,
        &[
            Some(r"{\b1}ab{\r}"),
            Some(r"ab{\b1}"),
            Some(r"ab{\1c&H000000&}{\2c&HFFFFFF&}{\1c&HFFFFFF&}{\2c&HFFFFFF&}"),
            Some("ab"),
            Some(r"{\b1}"),
            Some(r"ab{\b1}"),
            Some(r"{\b1}ab"),
            Some("ab"),
            None,
        ],
    );
}

#[test]
fn record_order_overlap_and_ranges_beyond_the_text() {
    let plain = description(PLAIN, &[(1, b"Arial")]);
    let bold = |start, end| Record {
        face: 1,
        ..at(start, end)
    };
    let italic = |start, end| Record {
        face: 2,
        ..at(start, end)
    };
    let samples = [
        styled(b"abcd", &[bold(2, 9)], &[]),
        styled(b"abcd", &[bold(4, 6)], &[]),
        styled(b"abcd", &[bold(0, 1), at(1, 2), bold(2, 3)], &[]),
        styled(b"abcd", &[bold(0, 1), italic(1, 1), bold(1, 2)], &[]),
        styled(b"abcd", &[bold(2, 3), italic(0, 1)], &[]),
        styled(b"abcd", &[at(0, 3), bold(1, 2)], &[]),
        styled(
            b"abcd",
            &[
                bold(0, 2),
                Record {
                    size: 20,
                    ..bold(2, 4)
                },
            ],
            &[],
        ),
        styled(
            b"abcd",
            &[Record {
                font: 9,
                ..at(1, 3)
            }],
            &[],
        ),
        styled(b"a\nb\r\nc", &[bold(1, 4)], &[]),
    ];
    expect(
        Some(&plain),
        &samples,
        &[
            Some(r"ab{\b1}cd"),
            Some("abcd"),
            Some(r"{\b1}a{\r}b{\b1}c{\r}d"),
            Some(r"{\b1}ab{\r}cd"),
            Some("abcd"),
            Some(r"a{\b1}b{\r}cd"),
            Some(r"{\b1}ab{\r}{\b1}{\fs20}cd"),
            Some(r"abc{\r}d"),
            Some(r"a{\b1}\Nb{\r}\Nc"),
        ],
    );
}
